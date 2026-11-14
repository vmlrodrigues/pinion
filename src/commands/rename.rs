use super::{io, process, Runner, SubCommand};
use std::io::Write;

impl Runner<'_, '_> {
    pub fn rename(&mut self, cmd: &SubCommand) {
        match cmd {
            SubCommand::Rename {
                tags,
                editor_tag,
                expected_account,
                action_json,
            } => {
                if let Some(tag) = editor_tag {
                    self.open_editor(tag, expected_account.as_deref());
                } else {
                    self.run(tags, expected_account.as_deref(), *action_json);
                }
            }
            _ => unreachable!(),
        }
    }

    fn open_editor(&self, tag: &str, expected_account: Option<&str>) {
        let config = self.config.as_ref().unwrap();
        let account = match config.pinboard_username() {
            Ok(account) => account,
            Err(error) => return action_error(&error.to_string()),
        };
        if expected_account.is_some_and(|selected| selected != account) {
            return action_error("The Pinboard account changed. Select the tag again.");
        }
        let context = serde_json::json!({
            "alfredworkflow": {
                "arg": "",
                "variables": {
                    "oldtag": tag,
                    "pinion_rename_account": account,
                    "success": "true",
                },
            },
        });
        println!("{context}");
    }

    fn run(&mut self, tags: &[String], expected_account: Option<&str>, action_json: bool) {
        match self.rename_tag(tags, expected_account) {
            Ok(()) => {
                let message = "Successfully renamed tag.";
                if action_json {
                    write_action_result(message, true);
                } else {
                    io::stdout()
                        .write_all(message.as_bytes())
                        .expect("Couldn't write to stdout");
                }
                if self.config.as_ref().unwrap().auto_update_cache {
                    self.refresh_after_write();
                }
            }
            Err(error) if action_json => action_error(&error.to_string()),
            Err(error) => rename_failed(&error.to_string()),
        }
    }

    fn rename_tag(
        &self,
        tags: &[String],
        expected_account: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("running rename::run");
        let [source, target] = tags else {
            return Err("Enter two tags.".into());
        };
        if source.is_empty() || target.is_empty() {
            return Err("Enter two tags.".into());
        }
        let config = self.config.as_ref().unwrap();
        if expected_account
            .is_some_and(|selected| config.pinboard_username().ok() != Some(selected))
        {
            return Err("The Pinboard account changed. Select the tag again.".into());
        }

        debug!("  calling rename API");
        let _mutation = self.lock_bookmark_mutations()?;
        self.pinboard.as_ref().unwrap().rename_tag(source, target)
    }
}

pub(crate) fn action_error(message: &str) {
    write_action_result(&error_message(message), false);
}

fn write_action_result(message: &str, success: bool) {
    let result = serde_json::json!({
        "alfredworkflow": {
            "arg": message,
            "variables": {"success": success.to_string()},
        },
    });
    println!("{result}");
}

fn error_message(message: &str) -> String {
    format!("Error: {}", crate::redact_token(message))
}

fn rename_failed(message: &str) -> ! {
    print!("{}", error_message(message));
    process::exit(1);
}
