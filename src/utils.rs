use std::{
	fs::File,
	io::{BufRead, BufReader, Read},
	path::{Path, PathBuf},
	sync::Arc,
};

use anyhow::{Result, bail};
use sequoia_openpgp::{fmt::hex, types::HashAlgorithm};

use crate::metadata::{AptMetadataHashAlgm, FileEntry};

pub fn get_reader(path: &dyn AsRef<Path>) -> Result<Box<BufReader<dyn Read>>> {
	let path = path.as_ref();
	let fd = File::options()
		.read(true)
		.write(false)
		.create(false)
		.open(path)?;
	let bufreader = BufReader::with_capacity(128 * 1024, fd);
	match path.extension() {
		Some(ext) => {
			if ext.eq_ignore_ascii_case("gz") {
				let decoder = flate2::bufread::GzDecoder::new(bufreader);
				Ok(Box::new(BufReader::new(decoder)))
			} else if ext.eq_ignore_ascii_case("xz") {
				let decoder = xz2::bufread::XzDecoder::new(bufreader);
				Ok(Box::new(BufReader::new(decoder)))
			} else {
				bail!("Unsupported file extension {:?}", ext);
			}
		}
		None => Ok(Box::new(bufreader)),
	}
}

/// Scan the mirror root, returns a list of files that is not present in the
/// mirror root.
/// *WARNING* We assume that every file in the local disk are identical to
/// the one in the remote. Checksuming hundreds of thousands of files is
/// VERY expensive. We only add it to the delta if either the file does not
/// exist, or the size of the file is not correct (like what rsync normally
/// does - checksums are performed if only it is instructed to do so).
pub fn scan_delta(root: &dyn AsRef<Path>, list: &Vec<FileEntry>) -> Vec<String> {
	let root = root.as_ref();
	let mut files = Vec::new();
	for f in list {
		let full_path = root.join(&f.path);
		if !full_path.exists() {
			files.push(f.path.clone());
		} else if let Ok(m) = full_path.metadata() {
			if m.len() != f.size {
				files.push(f.path.clone());
			}
		} else {
			files.push(f.path.clone());
		}
	}
	files
}

pub fn checksum_file(
	algm: AptMetadataHashAlgm,
	path: Arc<PathBuf>,
	expected: Arc<String>,
) -> Result<()> {
	let fd = File::options()
		.read(true)
		.write(false)
		.create(false)
		.open(path.as_path())?;
	let mut reader = BufReader::with_capacity(128 * 1024, fd);
	let mut hasher = HashAlgorithm::from(algm).context()?.for_digest();
	loop {
		let buf = reader.fill_buf()?;
		let len = buf.len();
		if len == 0 {
			break;
		}
		hasher.update(buf);
		reader.consume(len);
	}
	let mut digest = vec![0; hasher.digest_size()];
	hasher.digest(&mut digest)?;
	let hash_value = hex::encode(digest).to_ascii_lowercase();
	if hash_value != expected.to_ascii_lowercase() {
		bail!(
			"{:?} Checksum verification failed.\nExpected: {}\nActual:   {}",
			algm,
			expected,
			hash_value
		);
	}
	Ok(())
}
