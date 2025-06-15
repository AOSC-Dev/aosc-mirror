#![allow(dead_code)]

use std::{collections::HashMap, io::BufRead, path::PathBuf};

use anyhow::{Context, Result};
use log::{info, warn};

use crate::utils::get_reader;

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
fn parse_files_in_sources(path: PathBuf) -> Result<Vec<String>> {
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
	let mut tmp_filenames = Vec::new();
	let mut rel_path = String::new();
	for line in lines {
		let line = if let Ok(l) = line {
			l
		} else {
			continue;
		};

		// An empty line is a 'paragraph' divisor
		if line.is_empty() {
			// Process the files parsed from the last paragraph
			let full_paths = tmp_filenames
				.iter()
				.map(|name| format!("{}/{}", rel_path, name))
				.collect::<Vec<_>>();
			files.extend(full_paths);
			tmp_filenames.clear();
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
			let filename = line
				.split_whitespace()
				.last()
				.context("Invalid Sources entry")?
				.to_string();
			tmp_filenames.push(filename);
		}
	}
	Ok(files)
}

// Collect source tarballs, dsc files and debian packaging archives from Sources file.
pub async fn collect_source_files(
	dists: PathBuf,
	suites: HashMap<String, Vec<String>>,
	num_queues: u8,
) -> Result<Vec<String>> {
	let mut files = Vec::new();
	let mut sources_files = Vec::new();
	for suite in suites {
		let suite_name = suite.0;
		let suite_path = dists.join(&suite_name);
		for component in suite.1 {
			let mut source_path = suite_path.join(&component).join("source/Sources.gz");
			info!("Trying source file at {}", &source_path.display());
			if !source_path.is_file() {
				source_path = suite_path.join(&component).join("source/Sources.xz");
				info!("Trying source file at {}", &source_path.display());
			}
			if !source_path.is_file() {
				warn!(
					"Component {} in suite {} does not provide deb-src sources.",
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
	let mut handles = Vec::new();
	for queue in queues {
		handles.push(tokio::task::spawn_blocking(move || {
			use anyhow::Ok;
			let mut results: Vec<String> = Vec::new();
			for file in queue {
				let result = parse_files_in_sources(file)?;
				results.extend(result);
			}
			Ok(results)
		}));
	}
	for r in handles.into_iter() {
		files.extend(r.await??)
	}
	Ok(files)
}
