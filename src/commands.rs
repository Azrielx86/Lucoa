pub struct Command {
    pub section: &'static str,
    pub keys: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        section: "Normal",
        keys: "q / Ctrl-C",
        description: "quit",
    },
    Command {
        section: "Normal",
        keys: "o",
        description: "open file",
    },
    Command {
        section: "Normal",
        keys: "x",
        description: "close tab",
    },
    Command {
        section: "Normal",
        keys: "Tab / Shift-Tab",
        description: "next / previous tab",
    },
    Command {
        section: "Normal",
        keys: "1-9",
        description: "switch to tab",
    },
    Command {
        section: "Normal",
        keys: "/",
        description: "edit filter",
    },
    Command {
        section: "Normal",
        keys: "r / i",
        description: "toggle regex / case-insensitive filter",
    },
    Command {
        section: "Normal",
        keys: "m",
        description: "cycle view mode",
    },
    Command {
        section: "Normal",
        keys: "s",
        description: "search current view",
    },
    Command {
        section: "Normal",
        keys: "n / p",
        description: "next / previous search match",
    },
    Command {
        section: "Normal",
        keys: "v / V",
        description: "visual selection / line selection",
    },
    Command {
        section: "Normal",
        keys: "e",
        description: "toggle export",
    },
    Command {
        section: "Normal",
        keys: "w",
        description: "toggle wrap",
    },
    Command {
        section: "Normal",
        keys: "Arrows / PgUp / PgDn / Home / End",
        description: "move viewport",
    },
    Command {
        section: "Normal",
        keys: "?",
        description: "show help",
    },
    Command {
        section: "Search",
        keys: "Enter",
        description: "apply search",
    },
    Command {
        section: "Search",
        keys: "Esc",
        description: "cancel search",
    },
    Command {
        section: "Visual",
        keys: "h j k l",
        description: "move cursor",
    },
    Command {
        section: "Visual",
        keys: "w b e",
        description: "word motion",
    },
    Command {
        section: "Visual",
        keys: "y",
        description: "copy selection",
    },
    Command {
        section: "Visual",
        keys: "Esc",
        description: "cancel selection",
    },
    Command {
        section: "Prompt",
        keys: "Enter / Esc",
        description: "submit / cancel",
    },
    Command {
        section: "Prompt",
        keys: "Tab",
        description: "complete open path",
    },
    Command {
        section: "Prompt",
        keys: "Left / Right / Home / End",
        description: "move cursor",
    },
];
