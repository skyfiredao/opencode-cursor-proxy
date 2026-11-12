use crate::error::ApiError;

#[derive(Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ApiError> {
        let mut lines = Vec::new();
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            if self.pending.len() + part.len() > 8 * 1024 * 1024 {
                return Err(ApiError::upstream("cursor-agent NDJSON line exceeds 8 MiB"));
            }
            self.pending.extend_from_slice(part);
            if part.last() == Some(&b'\n')
                && let Some(line) = self.finish()?
            {
                lines.push(line);
            }
        }
        Ok(lines)
    }

    pub fn finish(&mut self) -> Result<Option<String>, ApiError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let bytes = std::mem::take(&mut self.pending);
        let line = String::from_utf8(bytes)
            .map_err(|_| ApiError::upstream("cursor-agent emitted invalid UTF-8"))?;
        let line = line.trim_end_matches(['\r', '\n']).to_owned();
        Ok((!line.trim().is_empty()).then_some(line))
    }
}
