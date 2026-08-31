use super::{io, process, PinBuilder, Runner, SubCommand};
use std::io::Write;

impl Runner<'_, '_> {
    pub fn retitle(&mut self, cmd: &SubCommand) {
        match cmd {
            SubCommand::Retitle { url, title } => self.run_retitle(url, title),
            _ => unreachable!(),
        }
    }

    /// Change an existing bookmark's title.
    ///
    /// Pinboard has no "edit" endpoint: `posts/add` with `replace: yes` — which is
    /// what `add_pin` sends — overwrites whatever is stored at that URL. So this is
    /// a read-modify-write, and every field we do not carry across would be
    /// silently erased. Hence the lookup first, and hence the refusal to proceed
    /// when the bookmark is not found.
    fn run_retitle(&mut self, url: &str, title: &[String]) {
        debug!("running retitle::run");
        let new_title = title.join(" ");
        let new_title = new_title.trim();

        if url.trim().is_empty() || new_title.is_empty() {
            crate::show_error_alfred("Need a URL and a new title.");
            process::exit(1);
        }

        // `find_url` hands back references into the cache, and `Pin` is not `Clone`,
        // so copy out the fields we need to carry over before touching `pinboard`
        // mutably below.
        let (tags, shared, toread, notes) = match self.pinboard.as_ref().unwrap().find_url(url) {
            Ok(Some(pins)) if !pins.is_empty() => {
                let p = pins[0];
                debug!("  retitling {:?} -> {:?}", p.title, new_title);
                (
                    p.tags.to_string(),
                    p.shared.to_string(),
                    p.toread.to_string(),
                    p.extended.as_ref().map(std::string::ToString::to_string),
                )
            }
            Ok(_) => {
                // Writing anyway would create a brand-new bookmark stripped of the
                // tags and notes the user expected to keep.
                crate::show_error_alfred(
                    "That bookmark isn't in your Pinboard cache. Try `pu` first.",
                );
                process::exit(1);
            }
            Err(e) => {
                crate::show_error_alfred(format!(
                    "Couldn't look up the bookmark: {}",
                    crate::redact_token(&e.to_string())
                ));
                process::exit(1);
            }
        };

        // Carry every field across except the title. `extended` is Pinboard's notes
        // field; its `description` field is the title, which is why PinBuilder takes
        // the title positionally and `.description()` sets the notes.
        let mut builder = PinBuilder::new(url, new_title)
            .tags(tags)
            .shared(shared)
            .toread(toread);
        if let Some(notes) = notes {
            builder = builder.description(notes);
        }

        match self.pinboard.as_mut().unwrap().add_pin(builder.into_pin()) {
            Ok(()) => {
                io::stdout()
                    .write_all(format!("Renamed to: {new_title}").as_bytes())
                    .expect("Couldn't write to stdout");
                if self.config.as_ref().unwrap().auto_update_cache {
                    self.update_cache(true);
                }
            }
            Err(e) => {
                io::stdout()
                    .write_all(format!("Error: {}", crate::redact_token(&e.to_string())).as_bytes())
                    .expect("Couldn't write to stdout");
                process::exit(1);
            }
        }
    }
}
