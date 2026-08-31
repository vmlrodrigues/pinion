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

        if url.trim().is_empty() {
            crate::show_error_alfred("Need a URL.");
            process::exit(1);
        }

        // With no new title this is a read: print what the bookmark is called now.
        // The workflow uses this to show the existing title and pre-fill it for
        // editing, so a one-character correction does not mean retyping the lot.
        let read_only = new_title.is_empty();

        // `find_url` hands back references into the cache, and `Pin` is not `Clone`,
        // so copy out the fields we need to carry over before touching `pinboard`
        // mutably below.
        let (current_title, tags, shared, toread, notes) =
            match self.pinboard.as_ref().unwrap().find_url(url) {
                Ok(Some(pins)) if !pins.is_empty() => {
                    let p = pins[0];
                    debug!("  current title {:?}", p.title);
                    (
                        p.title.to_string(),
                        p.tags.to_string(),
                        p.shared.to_string(),
                        p.toread.to_string(),
                        p.extended.as_ref().map(std::string::ToString::to_string),
                    )
                }
                Ok(_) => {
                    // A read is consumed by a script filter, which would otherwise
                    // treat an Alfred error item as the bookmark's title. Fail
                    // quietly and let the caller notice the exit code.
                    if read_only {
                        process::exit(1);
                    }
                    // Writing anyway would create a brand-new bookmark stripped of
                    // the tags and notes the user expected to keep.
                    crate::show_error_alfred(
                        "That bookmark isn't in your Pinboard cache. Try `pu` first.",
                    );
                    process::exit(1);
                }
                Err(e) => {
                    if read_only {
                        error!("retitle lookup: {}", crate::redact_token(&e.to_string()));
                        process::exit(1);
                    }
                    crate::show_error_alfred(format!(
                        "Couldn't look up the bookmark: {}",
                        crate::redact_token(&e.to_string())
                    ));
                    process::exit(1);
                }
            };

        if read_only {
            // Plain text, not an Alfred item: the script filter reads this on stdout.
            io::stdout()
                .write_all(current_title.as_bytes())
                .expect("Couldn't write to stdout");
            return;
        }

        if new_title == current_title {
            io::stdout()
                .write_all(b"Title unchanged.")
                .expect("Couldn't write to stdout");
            return;
        }

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
