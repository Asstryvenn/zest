//! `zest init`: an interactive, bilingual first-run setup. Zesty, the blue-fire
//! spirit, explains each step in plain language: what zest does, which mode to
//! pick, which port to use, how to point AI tools at it, and how to run it.
//!
//! The flow is generic over its input and output so it can be driven by a
//! script in tests.

use crate::compressor::Mode;
use crate::config::{self, Config};
use crate::tui::banner::{mascot_with, Mood};
use crate::tui::theme::{self, CORE, DEEP_BLUE, DEEP_CYAN, ELECTRIC, FLAME, MUTED, SPARK};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const STEPS: usize = 6;
const BUBBLE_WIDTH: usize = 52;
const BLOCK_START: &str = "# >>> zest >>>";
const BLOCK_END: &str = "# <<< zest <<<";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

impl Lang {
    fn t(self, en: &'static str, ru: &'static str) -> &'static str {
        match self {
            Lang::En => en,
            Lang::Ru => ru,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    pub fn from_code(s: &str) -> Option<Lang> {
        match s.trim().to_lowercase().as_str() {
            "en" | "english" | "2" => Some(Lang::En),
            "ru" | "russian" | "русский" | "1" => Some(Lang::Ru),
            _ => None,
        }
    }

    /// Russian when the system locale is Russian, English otherwise.
    pub fn from_env() -> Lang {
        let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default();
        if locale.to_lowercase().starts_with("ru") {
            Lang::Ru
        } else {
            Lang::En
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        match std::env::consts::OS {
            "macos" => Os::MacOs,
            "windows" => Os::Windows,
            _ => Os::Linux,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
    PowerShell,
    Sh,
}

impl Shell {
    pub fn detect(os: Os) -> Shell {
        if os == Os::Windows {
            return Shell::PowerShell;
        }
        let shell = std::env::var("SHELL").unwrap_or_default();
        match shell.rsplit('/').next().unwrap_or("") {
            "zsh" => Shell::Zsh,
            "bash" => Shell::Bash,
            "fish" => Shell::Fish,
            "" if os == Os::MacOs => Shell::Zsh,
            _ => Shell::Sh,
        }
    }

    /// The startup file new terminal tabs read. `None` for PowerShell, where
    /// `setx` is the beginner-friendly way to persist variables.
    pub fn profile(self, home: &Path, os: Os) -> Option<PathBuf> {
        match self {
            Shell::Zsh => Some(home.join(".zshrc")),
            Shell::Bash if os == Os::MacOs => Some(home.join(".bash_profile")),
            Shell::Bash => Some(home.join(".bashrc")),
            Shell::Fish => Some(home.join(".config").join("fish").join("config.fish")),
            Shell::Sh => Some(home.join(".profile")),
            Shell::PowerShell => None,
        }
    }

    /// Commands that set the two base URLs for the current terminal tab.
    pub fn env_lines(self, port: u16) -> Vec<String> {
        let (a, o) = urls(port);
        match self {
            Shell::Fish => vec![
                format!("set -gx ANTHROPIC_BASE_URL {a}"),
                format!("set -gx OPENAI_BASE_URL {o}"),
            ],
            Shell::PowerShell => vec![
                format!("$env:ANTHROPIC_BASE_URL = \"{a}\""),
                format!("$env:OPENAI_BASE_URL = \"{o}\""),
            ],
            _ => vec![format!("export ANTHROPIC_BASE_URL={a}"), format!("export OPENAI_BASE_URL={o}")],
        }
    }
}

fn urls(port: u16) -> (String, String) {
    (format!("http://127.0.0.1:{port}"), format!("http://127.0.0.1:{port}/v1"))
}

/// Everything the flow needs to know about the machine it runs on.
pub struct Setup {
    pub home: PathBuf,
    pub config_path: PathBuf,
    pub os: Os,
    pub shell: Shell,
    /// Skip the language screen (from `zest init --lang`).
    pub lang: Option<Lang>,
    pub default_lang: Lang,
    /// Clear the screen between steps (only when attached to a terminal).
    pub clear: bool,
    pub port_free: fn(u16) -> bool,
    /// Draw Zesty in 24-bit color (otherwise a plain "Zesty:" label).
    pub color: bool,
}

impl Setup {
    /// The setup for the real machine.
    pub fn detect(lang: Option<Lang>, clear: bool) -> Option<Setup> {
        let os = Os::current();
        Some(Setup {
            home: config::home_dir()?,
            config_path: config::path()?,
            os,
            shell: Shell::detect(os),
            lang,
            default_lang: Lang::from_env(),
            clear,
            port_free: |p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok(),
            color: theme::enabled(),
        })
    }
}

pub struct Outcome {
    pub config: Config,
    pub start_now: bool,
    /// The user asked for system-wide mode (macOS): trust the CA and turn on the system proxy.
    pub system_mode: bool,
}

/// Keyboard shortcuts, spelled the way each OS labels its keys.
struct Keys {
    new_tab: &'static [&'static str],
    copy: &'static [&'static str],
    paste: &'static [&'static str],
    palette: &'static [&'static str],
    settings: &'static [&'static str],
}

fn keys(os: Os) -> Keys {
    match os {
        Os::MacOs => Keys {
            new_tab: &["⌘ Cmd", "T"],
            copy: &["⌘ Cmd", "C"],
            paste: &["⌘ Cmd", "V"],
            palette: &["⌘ Cmd", "⇧ Shift", "P"],
            settings: &["⌘ Cmd", ","],
        },
        Os::Linux => Keys {
            new_tab: &["Ctrl", "Shift", "T"],
            copy: &["Ctrl", "Shift", "C"],
            paste: &["Ctrl", "Shift", "V"],
            palette: &["Ctrl", "Shift", "P"],
            settings: &["Ctrl", ","],
        },
        Os::Windows => Keys {
            new_tab: &["Ctrl", "Shift", "T"],
            copy: &["Ctrl", "C"],
            paste: &["Ctrl", "V"],
            palette: &["Ctrl", "Shift", "P"],
            settings: &["Ctrl", ","],
        },
    }
}

// ---------------------------------------------------------------- rendering

struct Ui<'a, W: Write> {
    out: &'a mut W,
    color: bool,
}

impl<W: Write> Ui<'_, W> {
    fn paint(&self, text: &str, c: theme::Rgb) -> String {
        if self.color {
            format!("{}{text}{}", theme::fg(c), theme::RESET)
        } else {
            text.to_string()
        }
    }

    fn bold(&self, text: &str, c: theme::Rgb) -> String {
        if self.color {
            format!("{}{}{text}{}", theme::BOLD, theme::fg(c), theme::RESET)
        } else {
            text.to_string()
        }
    }

    /// `[⌘ Cmd] + [T]` as raised keycaps.
    fn keys(&self, combo: &[&str]) -> String {
        combo
            .iter()
            .map(|k| {
                let k = nb(k);
                if self.color {
                    format!("{}{}{}\u{a0}{k}\u{a0}{}", theme::BOLD, theme::bg(DEEP_BLUE), theme::fg(CORE), theme::RESET)
                } else {
                    format!("[{k}]")
                }
            })
            .collect::<Vec<_>>()
            .join(&self.paint("\u{a0}+\u{a0}", MUTED))
    }

    fn line(&mut self, s: &str) -> io::Result<()> {
        writeln!(self.out, "{s}")
    }

    fn blank(&mut self) -> io::Result<()> {
        writeln!(self.out)
    }

    /// Clear the screen and draw the step header.
    fn header(&mut self, clear: bool, step: Option<usize>) -> io::Result<()> {
        if clear {
            write!(self.out, "\x1b[2J\x1b[H")?;
        }
        let mut h = format!("  {}", self.bold("zest", ELECTRIC));
        h.push_str(&self.paint(" · setup", MUTED));
        if let Some(n) = step {
            let dots: String = (1..=STEPS)
                .map(|i| if i <= n { self.paint("●", ELECTRIC) } else { self.paint("○", MUTED) })
                .collect::<Vec<_>>()
                .join(" ");
            h.push_str(&format!("   {dots}  {}", self.paint(&format!("{n}/{STEPS}"), MUTED)));
        }
        self.blank()?;
        self.line(&h)?;
        self.blank()
    }

    /// Zesty on the left, a speech bubble on the right.
    fn zesty(&mut self, mood: Mood, text: &str) -> io::Result<()> {
        let lines = wrap(text, BUBBLE_WIDTH);
        let border = |s: &str| self.paint(s, DEEP_CYAN);
        let mut bubble = vec![border(&format!("╭{}╮", "─".repeat(BUBBLE_WIDTH + 2)))];
        for l in &lines {
            let pad = BUBBLE_WIDTH - l.chars().count();
            bubble.push(format!("{} {}{} {}", border("│"), self.paint(l, CORE), " ".repeat(pad), border("│")));
        }
        bubble.push(border(&format!("╰{}╯", "─".repeat(BUBBLE_WIDTH + 2))));

        if !self.color {
            self.line("  Zesty:")?;
            for b in bubble {
                self.line(&format!("  {b}"))?;
            }
            return self.blank();
        }
        let art = mascot_with(mood);
        let tail_row = 3.min(bubble.len() - 2);
        for i in 0..art.len().max(bubble.len()) {
            let a = art.get(i).cloned().unwrap_or_else(|| " ".repeat(16));
            let gap = if i == tail_row { self.paint(" ◀", DEEP_CYAN) } else { "  ".to_string() };
            let b = bubble.get(i).map(String::as_str).unwrap_or("");
            self.line(&format!("  {a} {gap}{b}"))?;
        }
        self.blank()
    }

    /// A titled box of commands to copy.
    fn commands(&mut self, title: &str, cmds: &[String]) -> io::Result<()> {
        let width = cmds.iter().map(|c| c.chars().count()).max().unwrap_or(0).max(title.chars().count() + 2) + 2;
        let top = format!("╭─ {title} {}╮", "─".repeat(width - title.chars().count() - 3));
        self.line(&format!("     {}", self.paint(&top, FLAME)))?;
        for c in cmds {
            let pad = width - c.chars().count() - 1;
            self.line(&format!(
                "     {} {}{}{}",
                self.paint("│", FLAME),
                self.bold(c, SPARK),
                " ".repeat(pad),
                self.paint("│", FLAME)
            ))?;
        }
        self.line(&format!("     {}", self.paint(&format!("╰{}╯", "─".repeat(width)), FLAME)))?;
        self.blank()
    }

    fn numbered(&mut self, n: usize, title: &str, desc: &str) -> io::Result<()> {
        self.line(&format!("   {}  {}", self.bold(&n.to_string(), ELECTRIC), self.bold(title, CORE)))?;
        for l in wrap(desc, 66) {
            self.line(&format!("      {}", self.paint(&l, MUTED)))?;
        }
        self.blank()
    }

    fn heading(&mut self, s: &str) -> io::Result<()> {
        self.line(&format!("   {} {}", self.paint("▸", ELECTRIC), self.bold(s, SPARK)))
    }

    fn text(&mut self, s: &str) -> io::Result<()> {
        for l in wrap(s, 70) {
            self.line(&format!("     {}", self.paint(&l, CORE)))?;
        }
        Ok(())
    }

    fn ok(&mut self, s: &str) -> io::Result<()> {
        self.line(&format!("   {} {}", self.bold("✓", ELECTRIC), s))
    }

    fn warn(&mut self, s: &str) -> io::Result<()> {
        for (i, l) in wrap(s, 68).iter().enumerate() {
            let mark = if i == 0 { "!" } else { " " };
            self.line(&format!("   {} {}", self.bold(mark, FLAME), self.paint(l, CORE)))?;
        }
        Ok(())
    }
}

/// Non-breaking spaces, so `wrap` keeps a command or key combo on one line.
fn nb(s: &str) -> String {
    s.replace(' ', "\u{a0}")
}

/// Printed width: characters, not counting ANSI color codes.
fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_escape = false;
    for c in s.chars() {
        match (in_escape, c) {
            (false, '\x1b') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, _) => n += 1,
        }
    }
    n
}

/// Greedy word wrap on visible width; `\n` starts a new paragraph. Only ASCII
/// spaces break a line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut cur = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let needed = visible_len(&cur) + usize::from(!cur.is_empty()) + visible_len(word);
            if !cur.is_empty() && needed > width {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------- input

/// One trimmed line, or `None` on end of input or `q`.
fn ask<R: BufRead, W: Write>(input: &mut R, ui: &mut Ui<W>, prompt: &str) -> io::Result<Option<String>> {
    write!(ui.out, "   {} {} ", ui.bold("›", ELECTRIC), prompt)?;
    ui.out.flush()?;
    let mut buf = String::new();
    if input.read_line(&mut buf)? == 0 {
        writeln!(ui.out)?;
        return Ok(None);
    }
    let answer = buf.trim().to_string();
    if matches!(answer.to_lowercase().as_str(), "q" | "quit" | "exit" | "й") {
        return Ok(None);
    }
    Ok(Some(answer))
}

fn choose<R: BufRead, W: Write>(
    input: &mut R,
    ui: &mut Ui<W>,
    lang: Lang,
    prompt: &str,
    count: usize,
    default: usize,
) -> io::Result<Option<usize>> {
    loop {
        let Some(a) = ask(input, ui, prompt)? else { return Ok(None) };
        if a.is_empty() {
            return Ok(Some(default));
        }
        match a.parse::<usize>() {
            Ok(n) if (1..=count).contains(&n) => return Ok(Some(n)),
            _ => {
                let msg = match lang {
                    Lang::En => format!("Please type a number from 1 to {count}."),
                    Lang::Ru => format!("Введите число от 1 до {count}."),
                };
                ui.warn(&msg)?;
            }
        }
    }
}

fn yes_no<R: BufRead, W: Write>(input: &mut R, ui: &mut Ui<W>, prompt: &str, default: bool) -> io::Result<Option<bool>> {
    loop {
        let Some(a) = ask(input, ui, prompt)? else { return Ok(None) };
        match a.to_lowercase().as_str() {
            "" => return Ok(Some(default)),
            "y" | "yes" | "д" | "да" | "н" => return Ok(Some(true)),
            "n" | "no" | "нет" | "т" => return Ok(Some(false)),
            _ => ui.warn("y / n")?,
        }
    }
}

fn press_enter<R: BufRead, W: Write>(input: &mut R, ui: &mut Ui<W>, lang: Lang) -> io::Result<Option<()>> {
    let prompt = ui.paint(lang.t("Press Enter to continue", "Нажмите Enter, чтобы продолжить"), MUTED);
    Ok(ask(input, ui, &prompt)?.map(|_| ()))
}

/// Zesty's explanation of system-wide mode.
pub fn system_mode_text(lang: Lang) -> &'static str {
    lang.t(
        "System-wide mode: I become your Mac's proxy, so apps that call api.anthropic.com or api.openai.com with your own API key (VS Code extensions like Cline or Continue, scripts, your own apps) save tokens without changing any settings.\nIt needs your password once, to trust my own certificate, and that certificate only works for those two addresses. Chat apps like Claude Desktop, ChatGPT and Cursor's built-in AI use their own servers, so they won't change. Turn it off any time with zest mode cli.",
        "Системный режим: я становлюсь прокси вашего Mac, и приложения, которые обращаются к api.anthropic.com или api.openai.com с вашим API-ключом (расширения VS Code вроде Cline или Continue, скрипты, ваши приложения), экономят токены без каких-либо настроек.\nОдин раз понадобится пароль, чтобы доверить мой собственный сертификат, и он работает только для этих двух адресов. Чат-приложения вроде Claude Desktop, ChatGPT и встроенного ИИ Cursor используют свои серверы, поэтому для них ничего не изменится. Выключить: zest mode cli.",
    )
}

/// The system-mode question, exactly as asked during setup.
pub fn system_mode_question(lang: Lang) -> &'static str {
    lang.t(
        "Enable System-Wide Proxy for Desktop Apps? (Requires sudo for local Root CA trust) [y/N]",
        "Включить системный прокси для приложений? (Нужен sudo, чтобы доверить локальный корневой сертификат) [y/N]",
    )
}

/// Show Zesty saying `text` (used by commands outside the setup flow).
pub fn say<W: Write>(out: &mut W, color: bool, happy: bool, text: &str) -> io::Result<()> {
    let mut ui = Ui { out, color };
    ui.zesty(if happy { Mood::Happy } else { Mood::Normal }, text)
}

/// Ask a yes/no question with the setup's styling. `None` on end of input.
pub fn confirm<R: BufRead, W: Write>(input: &mut R, out: &mut W, color: bool, question: &str, default: bool) -> io::Result<Option<bool>> {
    let mut ui = Ui { out, color };
    let q = ui.bold(question, CORE);
    yes_no(input, &mut ui, &q, default)
}

// ---------------------------------------------------------------- shell profile

/// Insert or replace zest's block in a shell profile. Returns true if the file changed.
pub fn write_profile_block(path: &Path, lines: &[String]) -> io::Result<bool> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let block = format!("{BLOCK_START}\n{}\n{BLOCK_END}\n", lines.join("\n"));
    let updated = match (existing.find(BLOCK_START), existing.find(BLOCK_END)) {
        (Some(s), Some(e)) if e > s => {
            let end = e + BLOCK_END.len();
            let end = if existing[end..].starts_with('\n') { end + 1 } else { end };
            format!("{}{block}{}", &existing[..s], &existing[end..])
        }
        _ if existing.is_empty() => block,
        _ => {
            let sep = if existing.ends_with('\n') { "\n" } else { "\n\n" };
            format!("{existing}{sep}{block}")
        }
    };
    if updated == existing {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, updated)?;
    Ok(true)
}

fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

// ---------------------------------------------------------------- the flow

/// Run the whole setup. `Ok(None)` means the user quit before the end.
pub fn run<R: BufRead, W: Write>(input: &mut R, out: &mut W, s: &Setup) -> io::Result<Option<Outcome>> {
    let mut ui = Ui { out, color: s.color };
    let k = keys(s.os);

    // Language.
    let lang = match s.lang {
        Some(l) => l,
        None => {
            ui.header(s.clear, None)?;
            ui.zesty(
                Mood::Normal,
                "Hi! I'm Zesty, the little blue-fire spirit that lives inside zest.\nПривет! Я Зести, маленький дух синего огня, который живёт в zest.",
            )?;
            let default = if s.default_lang == Lang::Ru { 1 } else { 2 };
            let prompt = format!(
                "{} {}",
                ui.bold("Choose language / Выберите язык [1: Русский | 2: English]", CORE),
                ui.paint(&format!("[{default}]"), MUTED)
            );
            let Some(n) = choose(input, &mut ui, s.default_lang, &prompt, 2, default)? else { return Ok(None) };
            if n == 1 {
                Lang::Ru
            } else {
                Lang::En
            }
        }
    };
    let t = |en, ru| lang.t(en, ru);

    // 1. Welcome.
    ui.header(s.clear, Some(1))?;
    ui.zesty(
        Mood::Normal,
        t(
            "Nice to meet you! I'll set up zest in 6 short steps. Press Enter to accept the answer shown in [brackets], or type q to quit at any time.\nWhat zest does: when AI tools like Claude Code or Cursor talk to Anthropic or OpenAI, they send a lot of text: logs, whole files, the same line hundreds of times. You pay for every token. zest runs on your own computer, sits in the middle and trims the junk before it leaves, so the same question costs less.",
            "Приятно познакомиться! Я настрою zest за 6 коротких шагов. Нажмите Enter, чтобы принять ответ в [скобках], или введите q, чтобы выйти в любой момент.\nЧто делает zest: когда ИИ-инструменты вроде Claude Code или Cursor обращаются к Anthropic или OpenAI, они отправляют много текста: логи, целые файлы, одну и ту же строку сотни раз. Вы платите за каждый токен. zest работает на вашем компьютере, стоит посередине и убирает лишнее до отправки, поэтому тот же вопрос стоит дешевле.",
        ),
    )?;
    let flow = format!(
        "     {}  {}  {}  {}  {}",
        ui.bold(t("your AI tool", "ваш ИИ-инструмент"), CORE),
        ui.paint("──▶", ELECTRIC),
        ui.bold(t("zest (this computer)", "zest (этот компьютер)"), ELECTRIC),
        ui.paint("──▶", ELECTRIC),
        ui.bold("Anthropic / OpenAI", CORE)
    );
    ui.line(&flow)?;
    ui.line(&format!(
        "     {}",
        ui.paint(
            t(
                "Your API keys and code pass through unchanged in meaning and go nowhere new.",
                "API-ключи и код проходят без изменения смысла и никуда больше не отправляются."
            ),
            MUTED
        )
    ))?;
    ui.blank()?;
    if press_enter(input, &mut ui, lang)?.is_none() {
        return Ok(None);
    }

    // 2. Mode.
    ui.header(s.clear, Some(2))?;
    ui.zesty(
        Mood::Normal,
        t(
            "First: how hard should I squeeze? Think of it as a volume knob. Not sure? Pick 2, it's what most people use. You can change it later.",
            "Сначала: насколько сильно сжимать? Представьте ручку громкости. Не уверены — выберите 2, так делает большинство. Потом можно поменять.",
        ),
    )?;
    ui.numbered(
        1,
        t("Safe", "Безопасный (safe)"),
        t(
            "Only removes invisible junk: terminal colors, extra spaces, repeated lines. Never changes the meaning.",
            "Убирает только невидимый мусор: цвета терминала, лишние пробелы, повторы строк. Смысл не меняется никогда.",
        ),
    )?;
    ui.numbered(
        2,
        t("Balanced (recommended)", "Сбалансированный (balanced, рекомендуется)"),
        t(
            "Also shortens logs and error traces, and hides code you didn't ask about. Usually saves 50-95% on logs.",
            "Ещё сокращает логи и трассировки ошибок и скрывает код, о котором вы не спрашивали. На логах обычно экономит 50-95%.",
        ),
    )?;
    ui.numbered(
        3,
        t("Max", "Максимальный (max)"),
        t(
            "Squeezes everything, including files your tools read. Biggest savings; the AI sees an outline of each function instead of its full code.",
            "Сжимает всё, включая файлы, которые читают инструменты. Максимальная экономия; ИИ видит «скелет» функций вместо их полного кода.",
        ),
    )?;
    let Some(m) = choose(input, &mut ui, lang, &format!("{} [2]:", t("Your choice", "Ваш выбор")), 3, 2)? else {
        return Ok(None);
    };
    let mode = [Mode::Safe, Mode::Balanced, Mode::Max][m - 1];

    // 3. Port.
    ui.header(s.clear, Some(3))?;
    ui.zesty(
        Mood::Normal,
        t(
            "Now a port. A port is like an apartment number on your computer: programs use it to find each other. zest will live at 127.0.0.1 (that means \"this computer\") on the port you pick. 8080 is a good choice; change it only if another program already uses it.",
            "Теперь порт. Порт — это как номер квартиры на вашем компьютере: по нему программы находят друг друга. zest будет жить по адресу 127.0.0.1 (это значит «этот компьютер») на выбранном порту. 8080 — хороший вариант; меняйте, только если его уже занимает другая программа.",
        ),
    )?;
    let port = loop {
        let Some(a) = ask(input, &mut ui, &format!("{} [8080]:", t("Port", "Порт")))? else { return Ok(None) };
        let port = if a.is_empty() { 8080 } else {
            match a.parse::<u16>() {
                Ok(p) if p >= 1024 => p,
                _ => {
                    ui.warn(t("Please type a number from 1024 to 65535.", "Введите число от 1024 до 65535."))?;
                    continue;
                }
            }
        };
        if (s.port_free)(port) {
            ui.ok(&match lang {
                Lang::En => format!("Port {port} is free."),
                Lang::Ru => format!("Порт {port} свободен."),
            })?;
            break port;
        }
        ui.warn(&match lang {
            Lang::En => format!("Port {port} is in use right now (maybe zest is already running?)."),
            Lang::Ru => format!("Порт {port} сейчас занят (может, zest уже запущен?)."),
        })?;
        let Some(keep) = yes_no(input, &mut ui, t("Use it anyway? [y/N]", "Всё равно использовать? [y/N]"), false)? else {
            return Ok(None);
        };
        if keep {
            break port;
        }
    };
    ui.blank()?;

    // 4. Environment variables.
    ui.header(s.clear, Some(4))?;
    let lines = s.shell.env_lines(port);
    let profile = s.shell.profile(&s.home, s.os);
    let profile_name = profile.as_deref().map(|p| tilde(p, &s.home));
    let bubble = match (lang, &profile_name) {
        (Lang::En, Some(f)) => format!(
            "Your AI tools need to know where zest lives. They read two settings called environment variables: ANTHROPIC_BASE_URL (for Claude) and OPENAI_BASE_URL (for OpenAI). I can add them to {f}, the file every new terminal tab reads when it opens.\nGood to know: once they're set, zest has to be running whenever you use those tools, or they'll say \"connection refused\"."
        ),
        (Lang::Ru, Some(f)) => format!(
            "Вашим ИИ-инструментам нужно знать, где живёт zest. Они читают две настройки — переменные окружения: ANTHROPIC_BASE_URL (для Claude) и OPENAI_BASE_URL (для OpenAI). Я могу добавить их в {f} — этот файл читает каждая новая вкладка терминала.\nВажно: когда они заданы, zest должен быть запущен, пока вы пользуетесь этими инструментами, иначе они напишут «connection refused»."
        ),
        (Lang::En, None) => "Your AI tools need to know where zest lives. They read two settings called environment variables: ANTHROPIC_BASE_URL (for Claude) and OPENAI_BASE_URL (for OpenAI). Below are the commands for PowerShell.\nGood to know: once they're set, zest has to be running whenever you use those tools, or they'll say \"connection refused\".".to_string(),
        (Lang::Ru, None) => "Вашим ИИ-инструментам нужно знать, где живёт zest. Они читают две настройки — переменные окружения: ANTHROPIC_BASE_URL (для Claude) и OPENAI_BASE_URL (для OpenAI). Ниже — команды для PowerShell.\nВажно: когда они заданы, zest должен быть запущен, пока вы пользуетесь этими инструментами, иначе они напишут «connection refused».".to_string(),
    };
    ui.zesty(Mood::Normal, &bubble)?;
    ui.commands(t("the two settings", "две настройки"), &lines)?;

    let paste_steps = |ui: &mut Ui<W>, target: &str| -> io::Result<()> {
        ui.heading(t("How to paste them yourself", "Как вставить их самостоятельно"))?;
        ui.text(&format!("1. {}", t("Select the lines in the box above with your mouse.", "Выделите строки в рамке выше мышкой.")))?;
        ui.text(&format!("2. {} {}", t("Copy them:", "Скопируйте:"), ui.keys(k.copy)))?;
        ui.text(&format!("3. {} {} {}", target, ui.keys(k.paste), t("then press Enter.", "и нажмите Enter.")))?;
        ui.blank()
    };

    match profile {
        Some(path) => {
            let f = profile_name.clone().unwrap_or_default();
            let add = match lang {
                Lang::En => format!("Add them to {f} for me (recommended)"),
                Lang::Ru => format!("Добавь их в {f} за меня (рекомендуется)"),
            };
            ui.numbered(1, &add, t("Every new terminal tab will have them automatically.", "Каждая новая вкладка терминала получит их автоматически."))?;
            ui.numbered(
                2,
                t("I'll paste them myself", "Я вставлю их сам(а)"),
                t("They'll only work in the tab where you paste them, until you close it.", "Они будут работать только во вкладке, куда вы их вставите, пока она открыта."),
            )?;
            let Some(n) = choose(input, &mut ui, lang, &format!("{} [1]:", t("Your choice", "Ваш выбор")), 2, 1)? else {
                return Ok(None);
            };
            if n == 1 {
                match write_profile_block(&path, &lines) {
                    Ok(_) => {
                        ui.ok(&match lang {
                            Lang::En => format!("Added to {f}."),
                            Lang::Ru => format!("Добавлено в {f}."),
                        })?;
                        ui.text(&format!(
                            "{} {}",
                            t("They apply to tabs opened from now on. Open one with", "Они действуют во вкладках, открытых с этого момента. Новая вкладка:"),
                            ui.keys(k.new_tab)
                        ))?;
                        ui.text(t(
                            "To undo, delete the lines between \"# >>> zest >>>\" and \"# <<< zest <<<\" in that file.",
                            "Чтобы отменить, удалите строки между «# >>> zest >>>» и «# <<< zest <<<» в этом файле.",
                        ))?;
                        ui.blank()?;
                    }
                    Err(e) => {
                        ui.warn(&match lang {
                            Lang::En => format!("Couldn't write {f}: {e}. Paste the lines yourself instead:"),
                            Lang::Ru => format!("Не удалось записать {f}: {e}. Вставьте строки вручную:"),
                        })?;
                        paste_steps(&mut ui, t("Paste them into your terminal:", "Вставьте их в терминал:"))?;
                    }
                }
            } else {
                paste_steps(
                    &mut ui,
                    t("Paste them into the terminal tab where you'll run your AI tool:", "Вставьте их во вкладку терминала, где будете запускать ИИ-инструмент:"),
                )?;
                ui.text(&match lang {
                    Lang::En => format!("To keep them for good, paste the same lines at the end of {f}."),
                    Lang::Ru => format!("Чтобы они сохранились навсегда, вставьте те же строки в конец {f}."),
                })?;
                ui.blank()?;
            }
        }
        None => {
            paste_steps(&mut ui, t("Paste them into PowerShell:", "Вставьте их в PowerShell:"))?;
            let (a, o) = urls(port);
            ui.heading(t("To keep them for every new window", "Чтобы они были в каждом новом окне"))?;
            ui.commands("setx", &[format!("setx ANTHROPIC_BASE_URL \"{a}\""), format!("setx OPENAI_BASE_URL \"{o}\"")])?;
            ui.text(t(
                "setx only affects windows opened afterwards, so open a new one after running it.",
                "setx действует только на окна, открытые после него, поэтому затем откройте новое окно.",
            ))?;
            ui.blank()?;
        }
    }
    if press_enter(input, &mut ui, lang)?.is_none() {
        return Ok(None);
    }

    // 5. Tools and editors.
    ui.header(s.clear, Some(5))?;
    ui.zesty(
        Mood::Normal,
        t(
            "Almost done! Here's how to point your tools at zest. Terminal tools work right away. Editors need one small step.",
            "Почти готово! Вот как направить инструменты через zest. Терминальные работают сразу, редакторам нужен один маленький шаг.",
        ),
    )?;
    let (a_url, o_url) = urls(port);
    let echo = if s.shell == Shell::PowerShell { "echo $env:ANTHROPIC_BASE_URL" } else { "echo $ANTHROPIC_BASE_URL" };

    ui.heading(t("Claude Code and other terminal tools", "Claude Code и другие терминальные инструменты"))?;
    ui.text(&format!(
        "{} {}",
        t("Run them in a new terminal tab:", "Запускайте их в новой вкладке терминала:"),
        ui.keys(k.new_tab)
    ))?;
    ui.text(&format!("{} {}", t("Check it worked:", "Проверка:"), ui.bold(&nb(echo), SPARK)))?;
    ui.text(&format!("{} {}", t("should print", "должно вывести"), ui.paint(&a_url, SPARK)))?;
    ui.blank()?;

    ui.heading("VS Code")?;
    ui.text(t(
        "Open VS Code from a terminal tab that has the settings, so it inherits them:",
        "Откройте VS Code из вкладки терминала, где заданы настройки, — он их унаследует:",
    ))?;
    ui.text(&ui.bold(&nb("cd your-project && code ."), SPARK))?;
    ui.text(&format!(
        "{} {} {}",
        t("If \"code\" isn't found: in VS Code press", "Если команда code не найдена: в VS Code нажмите"),
        ui.keys(k.palette),
        t(
            "and run \"Shell Command: Install 'code' command in PATH\".",
            "и выполните «Shell Command: Install 'code' command in PATH»."
        )
    ))?;
    ui.text(&match lang {
        Lang::En => format!("Extensions with their own base-URL setting (Cline, Continue and others): use {a_url} for Anthropic, or {o_url} for OpenAI."),
        Lang::Ru => format!("Расширения со своей настройкой base URL (Cline, Continue и другие): укажите {a_url} для Anthropic или {o_url} для OpenAI."),
    })?;
    ui.blank()?;

    ui.heading("Cursor")?;
    ui.text(&format!(
        "{} {} {}",
        t("Open settings with", "Откройте настройки:"),
        ui.keys(k.settings),
        t("then go to Models > API Keys.", "затем Models > API Keys.")
    ))?;
    ui.text(&match lang {
        Lang::En => format!("Enter your own OpenAI key, turn on \"Override OpenAI Base URL\" and set it to {o_url}. Menu names can differ between Cursor versions."),
        Lang::Ru => format!("Введите свой ключ OpenAI, включите «Override OpenAI Base URL» и укажите {o_url}. Названия пунктов меню могут отличаться в разных версиях Cursor."),
    })?;
    ui.warn(t(
        "Cursor may send these requests from its own servers, which can't reach your computer. If it can't connect, use zest with terminal tools instead.",
        "Cursor может отправлять эти запросы со своих серверов, которые не видят ваш компьютер. Если подключиться не удаётся, используйте zest с терминальными инструментами.",
    ))?;
    ui.blank()?;

    // Optional system-wide mode (macOS): no per-app settings at all.
    let mut system_mode = false;
    if s.os == Os::MacOs {
        ui.zesty(Mood::Normal, system_mode_text(lang))?;
        let q = ui.bold(system_mode_question(lang), CORE);
        let Some(yes) = yes_no(input, &mut ui, &q, false)? else { return Ok(None) };
        system_mode = yes;
        ui.blank()?;
    } else if press_enter(input, &mut ui, lang)?.is_none() {
        return Ok(None);
    }

    // 6. Done.
    // System mode is switched on after setup, once the certificate is actually trusted.
    let off = (s.os == Os::MacOs && !system_mode).then_some(false);
    let cfg = Config {
        language: Some(lang.code().to_string()),
        mode: Some(mode),
        port: Some(port),
        mitm: off,
        system_proxy: off,
    };
    config::save_to(&cfg, &s.config_path)?;
    ui.header(s.clear, Some(6))?;
    let cfg_name = tilde(&s.config_path, &s.home);
    ui.zesty(
        Mood::Happy,
        &match lang {
            Lang::En => format!("All set! I saved your answers in {cfg_name}.\nYour everyday routine: keep zest running in one terminal tab and use your AI tools in another. After every request I print a line showing how many tokens I saved you. Run zest init any time to change your answers."),
            Lang::Ru => format!("Готово! Я сохранил ваши ответы в {cfg_name}.\nКаждый день: держите zest запущенным в одной вкладке терминала, а ИИ-инструменты используйте в другой. После каждого запроса я пишу строку о том, сколько токенов сэкономил. Запустите zest init, чтобы поменять ответы."),
        },
    )?;
    let tab1 = t("Tab 1", "Вкладка 1");
    let tab2 = t("Tab 2", "Вкладка 2");
    let col: usize = 30;
    let pad = |s: &str| format!("{s}{}", " ".repeat(col.saturating_sub(s.chars().count())));
    let edge = |ui: &Ui<W>, s: &str| ui.paint(s, DEEP_CYAN);
    ui.line(&format!(
        "     {}{}",
        edge(&ui, &format!("╭─ {tab1} {}╮", "─".repeat(col - tab1.chars().count() - 2))),
        edge(&ui, &format!("╭─ {tab2} {}╮", "─".repeat(col - tab2.chars().count() - 2)))
    ))?;
    let rows = [
        ("$ zest serve".to_string(), "$ claude".to_string()),
        (format!("{} :{port}", t("listening on", "слушаю порт")), t("(or any AI tool)", "(или любой ИИ-инструмент)").to_string()),
    ];
    for (l, r) in rows {
        ui.line(&format!(
            "     {} {}{}{} {}{}",
            edge(&ui, "│"),
            ui.bold(&pad(&l), SPARK),
            edge(&ui, "│"),
            edge(&ui, "│"),
            ui.bold(&pad(&r), CORE),
            edge(&ui, "│")
        ))?;
    }
    ui.line(&format!(
        "     {}{}",
        edge(&ui, &format!("╰{}╯", "─".repeat(col + 1))),
        edge(&ui, &format!("╰{}╯", "─".repeat(col + 1)))
    ))?;
    ui.text(&format!("{} {}", t("New tab:", "Новая вкладка:"), ui.keys(k.new_tab)))?;
    ui.blank()?;
    ui.line(&format!(
        "     {} {}",
        ui.paint(t("You'll see lines like:", "Вы увидите строки вроде:"), MUTED),
        ui.paint("[zest] 15,939 -> 692 tokens (-95.7%) | Saved: ~$0.076", SPARK)
    ))?;
    ui.blank()?;
    let Some(start_now) = yes_no(input, &mut ui, t("Start zest now? [Y/n]", "Запустить zest сейчас? [Y/n]"), true)? else {
        return Ok(Some(Outcome { config: cfg, start_now: false, system_mode }));
    };
    if !start_now {
        ui.line(&format!(
            "   {} {}",
            ui.paint(t("See you! Start it later with", "До встречи! Запустить позже:"), MUTED),
            ui.bold("zest serve", SPARK)
        ))?;
    }
    Ok(Some(Outcome { config: cfg, start_now, system_mode }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(dir: &Path, lang: Option<Lang>) -> Setup {
        Setup {
            home: dir.to_path_buf(),
            config_path: dir.join(".config/zest/config.toml"),
            os: Os::MacOs,
            shell: Shell::Zsh,
            lang,
            default_lang: Lang::En,
            clear: false,
            port_free: |p| p != 8080,
            color: false,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zest-onboarding-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn drive(script: &str, s: &Setup) -> (Option<Outcome>, String) {
        let mut input = io::Cursor::new(script.as_bytes().to_vec());
        let mut out = Vec::new();
        let r = run(&mut input, &mut out, s).unwrap();
        (r, String::from_utf8(out).unwrap().replace('\u{a0}', " "))
    }

    #[test]
    fn english_flow_writes_profile_and_config() {
        let dir = tmp("en");
        std::fs::write(dir.join(".zshrc"), "alias ll='ls -la'\n").unwrap();
        // lang=2, Enter, mode=3, busy 8080 -> refuse, 9191, profile=1, Enter, Enter, start=n
        let (r, out) = drive("2\n\n3\n\nn\n9191\n1\n\n\nn\n", &setup(&dir, None));
        let o = r.expect("completed");
        assert_eq!(
            o.config,
            Config { language: Some("en".into()), mode: Some(Mode::Max), port: Some(9191), mitm: Some(false), system_proxy: Some(false) }
        );
        assert!(!o.system_mode);
        assert!(out.contains("Enable System-Wide Proxy for Desktop Apps? (Requires sudo for local Root CA trust) [y/N]"));
        assert!(!o.start_now);
        assert!(out.contains("Choose language / Выберите язык [1: Русский | 2: English]"));
        assert!(out.contains("Port 8080 is in use"));
        assert!(out.contains("[⌘ Cmd] + [T]"));
        let rc = std::fs::read_to_string(dir.join(".zshrc")).unwrap();
        assert_eq!(
            rc,
            "alias ll='ls -la'\n\n# >>> zest >>>\nexport ANTHROPIC_BASE_URL=http://127.0.0.1:9191\nexport OPENAI_BASE_URL=http://127.0.0.1:9191/v1\n# <<< zest <<<\n"
        );
        let saved: Config = toml::from_str(&std::fs::read_to_string(dir.join(".config/zest/config.toml")).unwrap()).unwrap();
        assert_eq!(saved, o.config);

        // Running again with another port replaces the block instead of appending.
        let (r, _) = drive("\n\n9292\n1\n\n\ny\n", &setup(&dir, Some(Lang::En)));
        assert!(r.unwrap().start_now);
        let rc = std::fs::read_to_string(dir.join(".zshrc")).unwrap();
        assert_eq!(rc.matches(BLOCK_START).count(), 1);
        assert!(rc.contains(":9292/v1") && !rc.contains(":9191"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn russian_flow_speaks_russian() {
        let dir = tmp("ru");
        let (r, out) = drive("1\n\n\n9000\n2\n\n\nn\n", &setup(&dir, None));
        let o = r.unwrap();
        assert_eq!(o.config.language.as_deref(), Some("ru"));
        assert_eq!(o.config.mode, Some(Mode::Balanced));
        assert!(out.contains("Безопасный (safe)"));
        assert_eq!(o.config.port, Some(9000));
        assert!(out.contains("Вставьте их во вкладку терминала"));
        assert!(!dir.join(".zshrc").exists(), "option 2 must not touch the profile");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_answers_are_asked_again_and_q_quits() {
        let dir = tmp("q");
        let (r, out) = drive("7\n2\n\nq\n", &setup(&dir, None));
        assert!(r.is_none());
        assert!(out.contains("Please type a number from 1 to 2."));
        assert!(!dir.join(".config/zest/config.toml").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn end_of_input_aborts_cleanly() {
        let dir = tmp("eof");
        assert!(drive("", &setup(&dir, None)).0.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn windows_gets_powershell_and_setx() {
        let dir = tmp("win");
        let mut s = setup(&dir, Some(Lang::En));
        s.os = Os::Windows;
        s.shell = Shell::PowerShell;
        s.port_free = |_| true;
        let (r, out) = drive("\n\n\n\n\n\n", &s);
        assert!(r.is_some());
        assert!(out.contains("$env:ANTHROPIC_BASE_URL = \"http://127.0.0.1:8080\""));
        assert!(out.contains("setx OPENAI_BASE_URL"));
        assert!(out.contains("[Ctrl] + [Shift] + [T]"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn system_mode_can_be_chosen_on_macos() {
        let dir = tmp("sys");
        let (r, out) = drive("2\n\n\n9393\n2\n\ny\nn\n", &setup(&dir, None));
        let o = r.unwrap();
        assert!(o.system_mode);
        assert_eq!(o.config.mitm, None, "switched on later, once the CA is trusted");
        let flat = out.split_whitespace().filter(|w| *w != "│").collect::<Vec<_>>().join(" ");
        assert!(flat.contains("Claude Desktop, ChatGPT and Cursor's built-in AI use their own servers"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keycaps_never_wrap() {
        let ui_out = &mut Vec::new();
        let ui = Ui { out: ui_out, color: true };
        let combo = ui.keys(keys(Os::MacOs).new_tab);
        let text = format!("They apply to tabs opened from now on. Open one with {combo}");
        let lines = wrap(&text, 50);
        assert!(lines.last().unwrap().contains(&combo), "{lines:?}");
        assert!(lines.iter().all(|l| visible_len(l) <= 50 || l.contains(&combo)));
    }

    #[test]
    fn wraps_cyrillic_by_characters() {
        let lines = wrap("Привет мир это проверка переноса строк", 12);
        assert!(lines.iter().all(|l| l.chars().count() <= 12), "{lines:?}");
    }
}
