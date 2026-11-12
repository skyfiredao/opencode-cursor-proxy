#[derive(Default)]
pub struct DeltaTracker {
    text: String,
    thinking: String,
}

impl DeltaTracker {
    pub fn text(&mut self, text: &str, partial: bool) -> String {
        Self::append(&mut self.text, text, partial)
    }

    pub fn thinking(&mut self, text: &str, partial: bool) -> String {
        Self::append(&mut self.thinking, text, partial)
    }

    fn append(emitted: &mut String, text: &str, partial: bool) -> String {
        let delta = if partial {
            text
        } else if let Some(rest) = text.strip_prefix(emitted.as_str()) {
            rest
        } else if emitted.starts_with(text) {
            ""
        } else {
            text
        }
        .to_owned();
        emitted.push_str(&delta);
        delta
    }
}
