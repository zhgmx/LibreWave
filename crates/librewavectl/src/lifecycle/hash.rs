use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const BUFFER_SIZE: usize = 64 * 1024;

#[derive(Default)]
pub struct Stream(Sha256);

impl Stream {
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    pub fn update(&mut self, value: &[u8]) {
        self.0.update(value);
    }

    pub fn update_file(&mut self, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        let mut buffer = vec![0; BUFFER_SIZE];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(length) => self.update(&buffer[..length]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    pub fn finish(self) -> String {
        format!("{:x}", self.0.finalize())
    }
}

pub fn bytes(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

pub fn file(path: &Path) -> io::Result<String> {
    let mut stream = Stream::new();
    stream.update_file(path)?;
    Ok(stream.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn incremental_updates_preserve_byte_boundaries() {
        let mut stream = Stream::new();
        stream.update(b"path");
        stream.update(&[0]);
        stream.update(b"file\0");
        stream.update(b"contents");
        stream.update(&[0xff]);
        assert_eq!(stream.finish(), bytes(b"path\0file\0contents\xff"));
    }

    #[test]
    fn file_hash_matches_the_same_bytes_across_multiple_buffers() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "librewave-hash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let content = (0..BUFFER_SIZE * 2 + 17)
            .map(|index| u8::try_from(index % 251).expect("fixture value is in range"))
            .collect::<Vec<_>>();
        std::fs::write(&path, &content).expect("write hash fixture");
        let actual = file(&path).expect("hash fixture");
        std::fs::remove_file(path).expect("remove hash fixture");
        assert_eq!(actual, bytes(&content));
    }
}
