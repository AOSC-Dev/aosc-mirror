use anyhow::{Context, Result, bail};
use axum::{
	Json,
	extract::{ConnectInfo, State},
	http::Response,
};
use chrono::prelude::*;
use log::{error, info, warn};
use reqwest::Client;
use std::{
	collections::{HashMap, HashSet},
	fs::{remove_dir_all, remove_file},
	net::SocketAddr,
	path::{Path, PathBuf},
	sync::Arc,
};
use tokio::{
	fs::{File, create_dir_all, symlink},
	io::{AsyncWriteExt, BufWriter},
	process::Command,
	sync::RwLock,
	task::JoinSet,
};
use url::Url;

use crate::{
	AppState,
	aosc::fetch_topics,
	config::OperationMode,
	debian::collect_source_files,
	metadata::{
		AptRepoReleaseInfo, download_metadata_files, fetch_manifest, get_files,
		split_inrelease,
	},
	server::{Status, SyncRequestBody, SyncRequestResponse},
	utils::scan_delta,
	verify::{PgpKeyringStore, verify_pgp_signature, verify_request_signature},
};

#[derive(Debug, Clone)]
pub struct SyncJob<'a> {
	pub rsync_url: &'a Url,
	pub http_url: &'a Url,
	pub mode: OperationMode,
	pub mirror_sources: bool,
	pub suites: Vec<String>,
	pub archs: Vec<String>,
	pub threads: u8,
	pub dst: &'a Path,
	pub timestamp: i64,
	pub keyring_store: &'a PgpKeyringStore,
	pub client: &'a Client,
}

#[axum::debug_handler]
pub async fn do_sync(
	ConnectInfo(addr): ConnectInfo<SocketAddr>,
	State(s): State<Arc<RwLock<AppState>>>,
	Json(payload): Json<SyncRequestBody>,
) -> Response<String> {
	info!("Got request from {}", addr);
	let s2 = s.clone();
	let lock = s2.read().await;
	if !lock.config.skip_verification {
		// Verify signatures
		if payload.signature.is_empty() {
			info!("Got empty signature, rejecting.");
			let res = SyncRequestResponse {
				status: Status::Failed,
				message: "Invalid signature".into(),
			};
			return Response::builder()
				.status(400)
				.body(serde_json::to_string_pretty(&res).unwrap())
				.unwrap();
		}
		let sig = &payload.signature;
		if verify_request_signature(
			&payload.timestamp.to_string(),
			sig,
			&lock.server_pubkeys,
		)
		.is_err()
		{
			info!("Got invalid signature, rejecting.");
			let res = SyncRequestResponse {
				status: Status::Failed,
				message: "Invalid signature".into(),
			};
			return Response::builder()
				.status(400)
				.body(serde_json::to_string_pretty(&res).unwrap())
				.unwrap();
		}
		info!("Signature verified.");
	} else {
		warn!("Testing mode is enabled! Skipping signature verification.");
	}
	if lock.syncing {
		info!("Sync is already started, rejecting.");
		let res = SyncRequestResponse {
			status: Status::Failed,
			message: "Sync job is already started".into(),
		};
		return Response::builder()
			.status(400)
			.body(serde_json::to_string_pretty(&res).unwrap())
			.unwrap();
	}
	let h = tokio::spawn(async move { do_sync_inner(s, payload.timestamp).await });
	if let Err(e) = lock.sender.send(h).await {
		error!("Can not send the handle to the consumer: {}", e);
		let res = SyncRequestResponse {
			status: Status::Failed,
			message: format!("Internal error: Unable to consume the spawned task: {}", e)
		};
		return Response::builder()
			.status(400)
			.body(serde_json::to_string_pretty(&res).unwrap())
			.unwrap();
	};
	drop(lock);

	let res = SyncRequestResponse {
		status: Status::Success,
		message: "Sync job started".into(),
	};
	Response::new(serde_json::to_string_pretty(&res).unwrap())
}

async fn fireup_rsync(rsync_url: Url, dst_root: PathBuf, file_list: PathBuf) -> Result<()> {
	let mut cmd = Command::new("rsync");
	cmd.args(["-R", "-r", "-v", "--no-motd"]);
	cmd.arg(format!("--files-from={}", file_list.display()));
	cmd.arg(rsync_url.to_string());
	cmd.arg(dst_root);
	let mut handle = cmd.spawn()?;
	handle.wait().await?;
	Ok(())
}

pub async fn do_sync_inner(s: Arc<RwLock<AppState>>, timestamp: i64) {
	let local: DateTime<Local> = Local::now();
	info!("Starting sync at {}", local);
	let mut lock = s.write().await;
	lock.syncing = true;
	let k = lock.keyring_store.clone();
	let c = lock.config.clone();
	let client = lock.client.clone();
	drop(lock);
	let suites = match c.mode {
		OperationMode::AOSC => {
			if !c.mirror_topics {
				vec!["stable".into()]
			} else {
				let mut topics = fetch_topics(client.clone())
					.await
					.context("Unable to fetch the topic manifest")
					.unwrap()
					.into_iter()
					.map(|x| x.name)
					.collect::<Vec<_>>();
				info!("Manifest has {} topics.", topics.len());
				topics.push("stable".into());
				topics
			}
		}
		OperationMode::Debian => c.suites.clone(),
	};
	let j = SyncJob {
		rsync_url: &c.mirror_url,
		http_url: &c.http_url,
		mode: c.mode,
		mirror_sources: c.mirror_sources,
		suites,
		archs: c.archs.clone(),
		dst: &c.mirror_root,
		threads: c.parallel_jobs,
		timestamp,
		keyring_store: &k,
		client: &client,
	};
	let mut status = Status::Success;
	let mut message = String::new();
	if let Err(e) = do_sync_inner2(j).await {
		status = Status::Failed;
		info!("Sync failed:");
		error!("{}", e);
		e.chain().skip(1).for_each(|e| error!("{}", e));
		e.chain().for_each(|e| {
			message.push_str(": ");
			message.push_str(&e.to_string());
		});
	}
	let mut lock = s.write().await;
	lock.syncing = false;
	let now: DateTime<Utc> = Utc::now();
	let now = now.timestamp();
	lock.last_sync_timestamp = now;
	lock.last_sync_status = status;
	lock.last_sync_message = message;
}

async fn do_sync_inner2(j: SyncJob<'_>) -> Result<()> {
	// Download manifests and metadata to dists-TIMESTAMP/SUITE.
	let manifests = download_metadata(&j).await?;

	let rsync_url = j.rsync_url.clone();
	let dst = j.dst.to_path_buf().clone();
	let cur_dists_dir = dst.join(format!("dists-{}", j.timestamp));
	let mut suites = HashMap::new();
	for manifest in &manifests {
		let components = manifest.components.clone();
		suites.insert(manifest.suite.clone(), components);
	}
	let archs = j.archs.clone();
	let suites2 = suites.clone();
	let mut files_collected =
		tokio::task::spawn_blocking(move || get_files(dst, suites2, archs, j.timestamp))
			.await??;
	if j.mode == OperationMode::Debian && j.mirror_sources {
		let mut source_files =
			collect_source_files(cur_dists_dir, suites, j.threads).await?;
		files_collected.append(&mut source_files);
	}

	let mut hashset: HashSet<String> = HashSet::with_capacity(files_collected.capacity());
	files_collected
		.iter()
		.map(|e| e.path.clone())
		.for_each(|e| {
			hashset.insert(e);
		});
	files_collected.sort_by_key(|e| e.path.clone());
	info!("Collected {} files in total.", files_collected.len());
	info!("Scanning for incremental deltas ...");
	// Scan the files for incremental deltas, concurrently.
	let mut scan_queues = Vec::new();
	let actual_threads = files_collected.len().clamp(1, j.threads.into());
	let each_size = files_collected.len().div_ceil(actual_threads as usize);
	files_collected
		.chunks(each_size)
		.for_each(|x| scan_queues.push(x.to_vec()));
	let mut delta = Vec::new();
	let mut tasks = JoinSet::new();
	for queue in scan_queues {
		let root = j.dst.to_owned();
		tasks.spawn_blocking(move || scan_delta(&root, &queue));
	}
	while let Some(task) = tasks.join_next().await {
		delta.extend(task?);
	}
	// We don't need the sizes any more.
	drop(files_collected);

	if !delta.is_empty() {
		info!("Scan complete. {} files to download.", delta.len());
		// Distribute files into N lists
		let mut queues = Vec::new();
		let each_size = delta.len().div_ceil(actual_threads as usize);
		delta.chunks(each_size)
			.for_each(|x| queues.push(x.to_vec()));

		// Generate file lists for rsync
		let tmp_dir = j.dst.join(".tmp");
		create_dir_all(&tmp_dir).await?;
		info!("Writing file lists to {} ...", tmp_dir.display());
		let mut filelists = Vec::new();
		for (idx, queue) in queues.into_iter().enumerate() {
			let path = tmp_dir.join(format!("files-{}-{}.txt", j.timestamp, idx + 1));
			let fd = File::options()
				.create(true)
				.truncate(true)
				.append(false)
				.write(true)
				.open(&path)
				.await?;
			let mut writer = BufWriter::with_capacity(128 * 1024, fd);
			for f in queue {
				writer.write_all(f.as_bytes())
					.await
					.context("Failed to write file lists")?;
				writer.write_all(b"\n").await?;
			}
			writer.flush().await?;
			filelists.push(path);
		}

		// Fire up N instances of rsync
		let mut handles = Vec::new();
		info!("Starting up {} rsync instances ...", j.threads);
		for list in filelists {
			let rsync_url = rsync_url.clone();
			let dst = j.dst.to_path_buf();
			handles.push(tokio::task::spawn(async move {
				fireup_rsync(rsync_url, dst, list).await
			}));
		}

		for r in handles.into_iter() {
			r.await??;
		}
	} else {
		info!("The mirror is up to date - nothing to download.");
	}
	// Update the symlink
	let symlink_dists = j.dst.join("dists");
	let tmp_dists = j.dst.join(format!("dists-{}", j.timestamp));
	if symlink_dists.is_symlink() {
		tokio::fs::remove_file(&symlink_dists)
			.await
			.context("Unable to remove the symlink")?;
	}
	info!("Linking dists to dists-{} ...", j.timestamp);
	symlink(tmp_dists, symlink_dists).await?;

	// Remove unused files
	let root = j.dst.to_path_buf();
	tokio::task::spawn_blocking(move || {
		remove_unused_files(root, j.timestamp, hashset)
	})
	.await??;

	let local: DateTime<Local> = Local::now();
	info!("Sync finished successfully at {}", local);
	Ok(())
}

fn remove_unused_files(
	root: PathBuf,
	cur_timestamp: i64,
	known_files: HashSet<String>,
) -> Result<()> {
	info!("Removing unused files ...");
	// Remove old dists
	for entry in walkdir::WalkDir::new(&root)
		.min_depth(1)
		.max_depth(1)
		.into_iter()
	{
		let entry = if let Ok(e) = entry {
			e
		} else {
			continue;
		};
		if !entry.file_type().is_dir() {
			continue;
		}
		let name = entry
			.file_name()
			.to_str()
			.context(format!("Invalid name: {}", entry.path().display()))?;
		if !name.starts_with("dists-") {
			continue;
		}
		if let Some(n) = name.strip_prefix("dists-") {
			let t = n.parse::<i64>();
			if let std::result::Result::Ok(t) = t {
				if t != cur_timestamp {
					info!("Removing old dists directory {} ...", name);
					remove_dir_all(entry.path()).context(format!(
						"Unable to remove directory {}",
						entry.path().display()
					))?;
				}
			}
		}
	}
	// Remove package files that is not known to us
	info!("Removing unused package files ...");
	let mut cnt: usize = 0;
	for entry in walkdir::WalkDir::new(root.join("pool"))
		.min_depth(1)
		.follow_links(false)
		.into_iter()
	{
		let entry = if let Ok(e) = entry {
			e
		} else {
			continue;
		};
		if !entry.file_type().is_file() {
			continue;
		}
		let rel = if let Ok(rel) = entry.path().strip_prefix(&root) {
			rel
		} else {
			continue;
		};
		let rel_str = if let Some(s) = rel.to_str() {
			s
		} else {
			warn!("Invalid file path {}", rel.display());
			continue;
		};
		if !known_files.contains(rel_str) {
			if let Err(e) = remove_file(entry.path()) {
				warn!("Unable to remove {}: {}", entry.path().display(), e);
				continue;
			}
			info!("Removed {}", rel_str);
			cnt += 1;
		}
	}
	info!("Removed {} files.", cnt);

	let tmpdir = root.join(".tmp");
	if tmpdir.is_dir() {
		info!("Removing temporary directory ...");
		remove_dir_all(&tmpdir).context(format!(
			"Failed to remove the temporary directory at {}",
			tmpdir.display()
		))?;
	}
	info!("Finished removing unused files.");
	drop(known_files);
	Ok(())
}

async fn download_metadata(j: &SyncJob<'_>) -> Result<Vec<AptRepoReleaseInfo>> {
	let mut manifests = Vec::new();
	for suite in &j.suites {
		let (inrelease_content, release) =
			fetch_manifest(j.http_url.clone(), suite.clone(), j.client).await?;
		let manifest = if let Some(s) = &inrelease_content {
			let (body, sig) = split_inrelease(&s);
			verify_pgp_signature(&body, &sig, j.keyring_store)?;
			// Also verify the signature of Release.
			if let Some((release, sig)) = &release {
				verify_pgp_signature(&release, &sig, j.keyring_store).context(
					"Failed to verify the authenticity of the Release file",
				)?;
			}
			AptRepoReleaseInfo::parse_from(&body)?
		} else if let Some(pair) = &release {
			let (release, sig) = pair;
			verify_pgp_signature(&release, &sig, j.keyring_store)?;
			AptRepoReleaseInfo::parse_from(&release)?
		} else {
			bail!("No InRelease or Release file provided");
		};
		// Save InRelease to the disk.
		download_metadata_files(
			j.http_url,
			&manifest,
			j.dst.to_path_buf(),
			j.timestamp,
			j.mode,
			j.threads.into(),
			j.client,
		)
		.await?;
		manifests.push(manifest);
		info!("Saving InRelease/Release ...");
		// Integrity of the metadata files are verified, let's save the InRelease and Release/Release.gpg.
		if let Some(s) = &inrelease_content {
			let path =
				j.dst.join(format!("dists-{}/{}/InRelease", j.timestamp, &suite));
			let mut fd = File::options()
				.create(true)
				.truncate(true)
				.write(true)
				.open(&path)
				.await?;
			fd.write_all(s.as_bytes()).await?;
			fd.flush().await?;
		}
		if let Some((content, sig)) = &release {
			let path =
				j.dst.join(format!("dists-{}/{}/Release", j.timestamp, &suite));
			let mut fd = File::options()
				.create(true)
				.truncate(true)
				.write(true)
				.open(&path)
				.await?;
			fd.write_all(content.as_bytes()).await?;
			let path =
				j.dst.join(format!("dists-{}/{}/Release.gpg", j.timestamp, &suite));
			let mut fd = File::options()
				.create(true)
				.truncate(true)
				.write(true)
				.open(&path)
				.await?;
			fd.write_all(sig.as_bytes()).await?;
			fd.flush().await?;
		}
	}
	Ok(manifests)
}
