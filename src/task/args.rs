use std::collections::HashMap;

use crate::core::error::{Result, fail};

/// The parsed `--key value` options and positional arguments of one command.
#[derive(Default)]
pub struct Options {
    values: HashMap<String, String>,
    pub positional: Vec<String>,
    pub rebuild: bool,
}

impl Options {
    /// Splits the arguments into the command and its options, with the rules of `agent-task`:
    /// every `--key` takes the next argument as its value, except the `--rebuild` flag.
    pub fn parse(args: &[String]) -> Result<(Option<String>, Self)> {
        let mut options = Self::default();
        let Some((command, rest)) = args.split_first() else {
            return Ok((None, options));
        };
        let mut index = 0;
        while index < rest.len() {
            let argument = &rest[index];
            index += 1;
            let Some(key) = argument.strip_prefix("--") else {
                options.positional.push(argument.clone());
                continue;
            };
            if key == "rebuild" {
                options.rebuild = true;
                continue;
            }
            match rest.get(index) {
                Some(value) if !value.is_empty() && !value.starts_with("--") => {
                    options.values.insert(key.to_owned(), value.clone());
                    index += 1;
                }
                _ => return fail(format!("--{key} needs a value")),
            }
        }
        Ok((Some(command.clone()), options))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The task id: `--id` or the first positional argument.
    pub fn id_argument(&self) -> Option<&str> {
        self.get("id")
            .or_else(|| self.positional.first().map(String::as_str))
    }
}
