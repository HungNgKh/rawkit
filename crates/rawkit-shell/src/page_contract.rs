//! The page and the shell agree on three vocabularies, and nothing but this
//! checks that they do.
//!
//! The panel is a string to the compiler. It names a cull action as `"pick"`,
//! a command as `"apply_preset"`, a field of the view as `view.in_target` — and
//! a rename on the Rust side leaves all three compiling, passing every other
//! test, and failing in the window the first time somebody presses the key.
//! That happened: the page went on saying `quick_toggle` for an afternoon after
//! the action had become `target_toggle`.
//!
//! So the page is read here as text and held against the types. It is a scrape,
//! not a parse, and deliberately strict about it: an `act(` whose first argument
//! holds no string literal fails the test rather than being skipped, because a
//! name this cannot see is a name this cannot check.

use crate::library::tests::{library_at, Scratch};
use crate::library::CullAction;

const PAGE: &str = include_str!("../ui/panel.html");
const SHELL: &str = include_str!("main.rs");

/// Where each call to `name(` starts its arguments, skipping longer identifiers
/// that merely end in it — `react(` is not `act(`.
fn calls<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("{name}(");
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(&open) {
        let start = from + at;
        from = start + open.len();
        let before = text[..start].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        found.push(&text[from..]);
    }
    found
}

/// The first argument of a call, as written: up to the first comma or closing
/// bracket that is not inside a nested bracket or a string.
fn first_argument(arguments: &str) -> &str {
    let mut depth = 0usize;
    let mut quoted: Option<char> = None;
    for (at, c) in arguments.char_indices() {
        match (quoted, c) {
            (Some(q), _) if c == q => quoted = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quoted = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') if depth == 0 => return &arguments[..at],
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => return &arguments[..at],
            _ => {}
        }
    }
    arguments
}

/// Every double-quoted literal in a stretch of source. A ternary holds two, and
/// both are names the shell has to know.
fn literals(source: &str) -> Vec<&str> {
    source.split('"').skip(1).step_by(2).collect()
}

/// The names the page hands to `name(`, and a complaint for any call that
/// hands over something this cannot read.
fn names_passed_to(name: &str) -> Vec<&'static str> {
    let mut names = Vec::new();
    for arguments in calls(PAGE, name) {
        let first = first_argument(arguments);
        // The definition itself — `const act = async (action, value) =>` is not
        // a call, and neither is a wrapper forwarding its own parameter.
        // `act(command.act, command.value)` is the registry carrying a command
        // out: the names it forwards are the `act: "…"` entries, read below.
        let forwarded = first
            .trim()
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
        if forwarded {
            continue;
        }
        let found = literals(first);
        assert!(
            !found.is_empty(),
            "`{name}({first}…` names nothing this test can read. Give it a string \
             literal, or teach the test the new shape — a name it cannot see is a \
             name it cannot check."
        );
        names.extend(found);
    }
    // The command registry names its actions as data, `act: "pick"`, rather than
    // as calls — and a name the scrape cannot see is a name nothing checks.
    if name == "act" {
        names.extend(registry().iter().filter_map(|command| command.act));
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// One entry of the page's command registry, as far as this needs to read it.
struct Registered {
    id: &'static str,
    act: Option<&'static str>,
    value: Option<&'static str>,
    waits: bool,
}

/// What comes after `key: ` in an entry, up to the comma or brace that ends it.
fn field(entry: &'static str, key: &str) -> Option<&'static str> {
    let at = entry.find(&format!("{key}: "))? + key.len() + 2;
    let rest = &entry[at..];
    let end = if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"')? + 2
    } else {
        rest.find([',', '}', '\n'])?
    };
    Some(rest[..end].trim().trim_matches('"'))
}

/// The registry, entry by entry. Every entry opens with `{ id: ` and nothing
/// else in the list does, which is what lets a scrape find them.
fn registry() -> Vec<Registered> {
    let list = PAGE
        .split_once("const COMMANDS = [")
        .and_then(|(_, rest)| rest.split_once("\n  ];"))
        .map(|(list, _)| list)
        .expect("the command registry");
    let entries: Vec<Registered> = list
        .split("{ id: ")
        .skip(1)
        .map(|entry| Registered {
            id: entry.split(['"', '`']).nth(1).unwrap_or(""),
            act: field(entry, "act"),
            value: field(entry, "value"),
            waits: match field(entry, "waits") {
                Some("true") => true,
                Some("false") => false,
                other => panic!("an entry says `waits: {other:?}`: {}", &entry[..60]),
            },
        })
        .collect();
    assert!(
        entries.len() > 40,
        "the scrape found {} commands, which is too few to be the real registry",
        entries.len()
    );
    entries
}

#[test]
fn every_action_the_page_sends_is_one_the_shell_knows() {
    let names = names_passed_to("act");
    assert!(
        names.len() > 20,
        "the scrape found {} actions, which is too few to be the real page",
        names.len()
    );
    for name in names {
        // Without a value, because the test cannot know what type each wants.
        // An action that needs one fails to deserialise for *that* reason, and
        // only "unknown variant" means the name itself is wrong.
        let sent = serde_json::json!({ "action": name });
        if let Err(why) = serde_json::from_value::<CullAction>(sent) {
            assert!(
                !why.to_string().contains("unknown variant"),
                "the page sends `{name}` and the shell has no such action: {why}"
            );
        }
    }
}

#[test]
fn every_command_the_page_invokes_is_registered() {
    let registered = SHELL
        .split_once("generate_handler![")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(list, _)| list)
        .expect("the handler list");
    let registered: Vec<&str> = registered
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    let names = names_passed_to("invoke");
    assert!(
        names.len() > 20,
        "the scrape found {} commands, which is too few to be the real page",
        names.len()
    );
    for name in names {
        assert!(
            registered.contains(&name),
            "the page invokes `{name}`, which is not in `generate_handler!`"
        );
    }
}

#[test]
fn every_field_the_page_reads_is_one_the_view_has() {
    let scratch = Scratch::new("page-contract");
    let library = library_at(&scratch.0, 3);
    let view = serde_json::to_value(library.view().unwrap()).unwrap();
    let has = view.as_object().expect("the view is an object");

    let mut read = Vec::new();
    for (at, _) in PAGE.match_indices("view.") {
        let before = PAGE[..at].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        let field: String = PAGE[at + "view.".len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !field.is_empty() {
            read.push(field);
        }
    }
    read.sort_unstable();
    read.dedup();
    assert!(
        read.len() > 10,
        "only {} fields found; the page reads more",
        read.len()
    );
    for field in read {
        assert!(
            has.contains_key(&field),
            "the page reads `view.{field}`, which the shell does not send"
        );
    }
}

/// An action by name, with whatever value makes it one. The page's `value` when
/// it is a literal; otherwise something of each shape, since the entries built
/// in a loop (`value: stars`) carry a variable this cannot read.
fn action(name: &str, value: Option<&str>) -> Option<CullAction> {
    let literal = value.and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok());
    let shapes = [
        literal,
        None,
        Some(serde_json::json!(1)),
        Some(serde_json::json!("red")),
        Some(serde_json::json!({})),
    ];
    shapes.into_iter().find_map(|value| {
        let sent = match value {
            Some(value) => serde_json::json!({ "action": name, "value": value }),
            None => serde_json::json!({ "action": name }),
        };
        serde_json::from_value(sent).ok()
    })
}

#[test]
fn the_page_and_the_shell_agree_about_what_waits_for_a_tool() {
    use crate::library::Tool;
    // The rule is held twice on purpose — the page names the exits, the shell
    // does not depend on a string — and two lists of one rule is two chances to
    // be wrong. A command the page lets through and the shell refuses is a key
    // that answers with a sentence naming no way out; the reverse is a key that
    // tells you to put down a tool the shell would not have minded.
    for command in registry() {
        let Some(name) = command.act else { continue };
        let action = action(name, command.value).unwrap_or_else(|| {
            panic!(
                "`{}` sends `{name}` with a value nothing here can guess",
                command.id
            )
        });
        let shell = [Tool::Crop, Tool::Spot, Tool::Placing]
            .into_iter()
            .any(|tool| action.waits_for(tool));
        assert_eq!(
            command.waits, shell,
            "`{}` ({name}): the page says waits = {}, and the shell's `waits_for` says {shell}",
            command.id, command.waits
        );
    }
}
