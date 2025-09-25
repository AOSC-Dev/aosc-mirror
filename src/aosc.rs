use std::{fs::{create_dir_all, File}, io::Write, path::PathBuf};

use anyhow::Result;
use log::info;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;

/// Represents a topic. Serializes to /var/lib/atm/state.
#[derive(Deserialize, Serialize, Clone)]
// arch and draft are not used
#[allow(dead_code)]
pub struct Topic {
	/// Topic name.
	pub name: String,
	/// Topic description.
	pub description: Option<String>,
	/// Date of the launch - as time64_t.
	pub date: i64,
	/// Update date of this topic - as time_t.
	pub update_date: i64,
	/// Available archs in this topic.
	pub arch: Vec<String>,
	/// Affected packages in this topic.
	pub packages: Vec<String>,
	/// Whether the corresponding PR is a draft.
	pub draft: bool,
}

pub async fn fetch_topics(
	mirror_url: &Url,
	dest: PathBuf,
	client: Client,
) -> Result<Vec<Topic>> {
	info!("Fetching topics manifest ...");
	let full_url = mirror_url.join("manifest/topics.json")?;
	let response = client.get(full_url).send().await?;
	response.error_for_status_ref()?;
	let content = &response.text().await?;
	let topics: Vec<Topic> = serde_json::from_str(content)?;
	let save_path = dest.join("manifest/");
	create_dir_all(&save_path)?;
	let save_path = save_path.join("topics.json");
	let mut fd = File::options()
		.create(true)
		.truncate(true)
		.write(true)
		.open(&save_path)?;
	fd.write_all(content.as_bytes())?;
	Ok(topics)
}
