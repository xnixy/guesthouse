//! Finding the machine a person means by the word they typed.

use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::machine::{Machine, load_all};

fn choices(machines: &[&Machine]) -> String {
    let each: Vec<String> =
        machines.iter().map(|machine| format!("{} ({})", machine.id, machine.name)).collect();
    each.join(", ")
}

/// The machine whose id is exactly `word`, or else the one whose name is, in any case. Two machines can
/// share a name, so a name can mean more than one, and then the ids are the way out.
pub(super) fn find(context: &Context, word: &str) -> Result<Machine> {
    let (machines, _) = load_all(context);
    if let Some(found) = machines.iter().find(|machine| machine.id == word) {
        return Ok(found.clone());
    }
    let lower = word.to_lowercase();
    let named: Vec<&Machine> =
        machines.iter().filter(|machine| machine.name.to_lowercase() == lower).collect();
    match named[..] {
        [only] => Ok(only.clone()),
        [] if machines.is_empty() => {
            Err(Failure::new("There are no machines yet. They are made in the app."))
        }
        [] => {
            let all: Vec<&Machine> = machines.iter().collect();
            Err(Failure(format!("There is no machine called {word}. The machines are {}.", choices(&all))))
        }
        _ => Err(Failure(format!("{word} could be any of {}. Use the id.", choices(&named)))),
    }
}
