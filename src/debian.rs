#![allow(dead_code)]

use std::{collections::HashMap, io::BufRead, path::PathBuf};

use anyhow::{Context, Result};
use log::warn;
use tokio::task::JoinSet;

use crate::{metadata::FileEntry, utils::get_reader};

#[allow(non_snake_case)]
struct TracingInfo {
	Date: String,
	Date_Started: String,
	Creator: String,
	Running_on_host: String,
	Maintainer: String,
	Suites: String,
	Architectures: String,
	Upstream_Mirror: String,
}

const TRACE_DIR: &str = "project/trace";

// Well, looks like we *have to* use deb822.
// Sources file uses collections for each source entry.
// Each entry in the collection is a filename, not relative path.
// Well, after a little bit of thinking, I think a partial parser would be suffice.
// Let's do it then!
fn parse_files_in_sources(path: PathBuf) -> Result<Vec<FileEntry>> {
	#[derive(Copy, Clone, PartialEq)]
	enum State {
		Normal,
		InParagraph,
		InFiles,
	}
	let mut files = Vec::new();
	let reader = get_reader(&path)?;

	let mut state = State::Normal;
	let lines = reader.lines();
	let mut tmp_files = Vec::new();
	let mut rel_path = String::new();
	for (idx, line) in lines.enumerate() {
		let line = if let Ok(l) = line {
			l
		} else {
			continue;
		};

		// An empty line is a 'paragraph' divisor
		if line.is_empty() {
			// Process the files parsed from the last paragraph
			let full_paths = tmp_files
				.iter()
				.map(|(entry, size)| FileEntry {
					path: format!("{}/{}", rel_path, entry),
					size: *size,
				})
				.collect::<Vec<_>>();
			files.extend(full_paths);
			tmp_files.clear();
			state = State::InParagraph;
			continue;
		}

		if state == State::InParagraph && line.starts_with("Files:") {
			state = State::InFiles;
			// Skip to the next line
			continue;
		}
		if state == State::InFiles && line.trim_start() == line {
			state = State::InParagraph;
		}

		if state == State::InParagraph && line.starts_with("Directory: ") {
			rel_path = line
				.split_whitespace()
				.last()
				.context("Invalid Sources entry")?
				.into();
			continue;
		}
		if state == State::InFiles {
			let mut fields = line.split_whitespace();
			let size = fields.nth(1).context(format!(
				"Expecting file size in a file entry in the Sources file {}:{}",
				path.display(),
				idx
			))?;
			let filename = fields.last().context("Invalid Sources entry")?.to_owned();
			let size: u64 = size.parse().context(format!(
				"Invalid size field in {}:{}",
				path.display(),
				idx
			))?;
			tmp_files.push((filename, size));
		}
	}
	Ok(files)
}

// Collect source tarballs, dsc files and debian packaging archives from Sources file.
pub async fn collect_source_files(
	dists: PathBuf,
	suites: HashMap<String, Vec<String>>,
	num_queues: u8,
) -> Result<Vec<FileEntry>> {
	let mut files = Vec::new();
	let mut sources_files = Vec::new();
	for suite in suites {
		let suite_name = suite.0;
		let suite_path = dists.join(&suite_name);
		for component in suite.1 {
			let mut source_path = suite_path.join(&component).join("source/Sources.gz");
			if !source_path.is_file() {
				source_path = suite_path.join(&component).join("source/Sources.xz");
			}
			if !source_path.is_file() {
				warn!(
					"Component {} in suite {} does not provide deb-src sourcs.",
					component, suite_name
				);
				continue;
			}
			sources_files.push(source_path);
		}
	}
	let num_queues = num_queues.clamp(1, num_queues);
	let mut queues = (1..=num_queues)
		.map(|_| Vec::<PathBuf>::new())
		.collect::<Vec<_>>();
	for (idx, sources_file) in sources_files.into_iter().enumerate() {
		let queue = &mut queues[idx % num_queues as usize];
		queue.push(sources_file);
	}
	let mut handles = JoinSet::new();
	for queue in queues {
		handles.spawn_blocking(move || {
			use anyhow::Ok;
			let mut results = Vec::new();
			for file in queue {
				let result = parse_files_in_sources(file)?;
				results.extend(result);
			}
			Ok(results)
		});
	}
	while let Some(r) = handles.join_next().await {
		files.append(&mut r??);
	}
	Ok(files)
}
