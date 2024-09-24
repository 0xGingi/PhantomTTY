use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::collections::VecDeque;
use std::os::unix::io::{FromRawFd, AsRawFd};

use eframe::egui;
use rustyline::{CompletionType, Config, EditMode, Editor};
use rustyline::completion::FilenameCompleter;
use rustyline::history::DefaultHistory;

use nix::pty::{openpty, Winsize};
use nix::unistd::{ForkResult, fork, setsid, Pid, tcsetpgrp};
use nix::sys::termios::{self, SetArg};
use nix::sys::select::{select, FdSet};
use nix::sys::time::TimeVal;
use nix::libc;

use vte::{Parser, Perform};
use vte::Params;

const HISTORY_SIZE: usize = 1000;

struct VteTerminal {
    screen: Vec<char>,
    cursor_x: usize,
    cursor_y: usize,
    width: usize,
    height: usize,
}

impl VteTerminal {
    fn new(width: usize, height: usize) -> Self {
        Self {
            screen: vec![' '; width * height],
            cursor_x: 0,
            cursor_y: 0,
            width,
            height,
        }
    }

    fn process(&mut self, data: &[u8]) {
        let mut parser = Parser::new();
        for (i, &byte) in data.iter().enumerate() {
            parser.advance(self, byte);
            if self.cursor_x >= self.width || self.cursor_y >= self.height {
                eprintln!("Warning: Cursor out of bounds at byte {} (x: {}, y: {})", i, self.cursor_x, self.cursor_y);
                self.cursor_x = self.cursor_x.min(self.width - 1);
                self.cursor_y = self.cursor_y.min(self.height - 1);
            }
        }
    }
    
    fn get_screen(&self) -> String {
        let mut output = String::with_capacity(self.width * self.height);
        for chunk in self.screen.chunks(self.width) {
            output.extend(chunk.iter());
            output.push('\n');
        }
        output
    }

    fn clear_screen(&mut self) {
        self.screen = vec![' '; self.width * self.height];
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor_y = row.min(self.height - 1);
        self.cursor_x = col.min(self.width - 1);
    }

    fn erase_in_line(&mut self, mode: usize) {
        let start = match mode {
            0 => self.cursor_y * self.width + self.cursor_x,
            1 => self.cursor_y * self.width,
            2 => 0,
            _ => return,
        };
        let end = (self.cursor_y + 1) * self.width;
        self.screen[start..end].fill(' ');
    }
}

impl Perform for VteTerminal {
    fn print(&mut self, c: char) {
        if self.cursor_x >= self.width {
            self.cursor_x = 0;
            self.cursor_y += 1;
        }
        if self.cursor_y >= self.height {
            self.screen.drain(0..self.width);
            self.screen.extend(std::iter::repeat(' ').take(self.width));
            self.cursor_y = self.height - 1;
        }
        let pos = self.cursor_y * self.width + self.cursor_x;
        if pos < self.screen.len() {
            self.screen[pos] = c;
        } else {
            eprintln!("Warning: Attempted to print outside screen bounds (x: {}, y: {})", self.cursor_x, self.cursor_y);
        }
        self.cursor_x += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor_x = 0,
            b'\n' => {
                self.cursor_y += 1;
                if self.cursor_y >= self.height {
                    self.screen.drain(0..self.width);
                    self.screen.extend(std::iter::repeat(' ').take(self.width));
                    self.cursor_y = self.height - 1;
                }
            },
            b'\x08' => if self.cursor_x > 0 { self.cursor_x -= 1 },
            b'\x0C' => self.clear_screen(),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    
    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, c: char) {
        let param = |idx: usize| -> usize {
            params.iter()
                .nth(idx)
                .and_then(|slice| slice.first())
                .map(|&x| x as usize)
                .unwrap_or(1)
        };

        match c {
            'A' => {
                let n = param(0);
                self.cursor_y = self.cursor_y.saturating_sub(n);
            }
            'B' => {
                let n = param(0);
                self.cursor_y = (self.cursor_y + n).min(self.height - 1);
            }
            'C' => {
                let n = param(0);
                self.cursor_x = (self.cursor_x + n).min(self.width - 1);
            }
            'D' => {
                let n = param(0);
                self.cursor_x = self.cursor_x.saturating_sub(n);
            }
            'H' | 'f' => {
                let row = param(0).saturating_sub(1);
                let col = param(1).saturating_sub(1);
                self.move_cursor(row, col);
            }
            'J' => {
                let mode = param(0);
                match mode {
                    0 => {
                        let start = self.cursor_y * self.width + self.cursor_x;
                        self.screen[start..].fill(' ');
                    }
                    1 => {
                        let end = self.cursor_y * self.width + self.cursor_x;
                        self.screen[..=end].fill(' ');
                    }
                    2 | 3 => self.clear_screen(),
                    _ => {}
                }
            }
            'K' => {
                let mode = param(0);
                self.erase_in_line(mode);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

struct PhantomCompleter {
    filename_completer: FilenameCompleter,
}

impl rustyline::completion::Completer for PhantomCompleter {
    type Candidate = rustyline::completion::Pair;

    fn complete(&self, line: &str, pos: usize, ctx: &rustyline::Context<'_>) 
        -> rustyline::Result<(usize, Vec<Self::Candidate>)> 
    {
        if line.starts_with("cd ") || line.contains('/') {
            self.filename_completer.complete(line, pos, ctx)
        } else {
            let commands = vec!["cd", "ls", "echo", "cat", "grep", "history", "exit"];
            let matches: Vec<Self::Candidate> = commands.iter()
                .filter(|&cmd| cmd.starts_with(&line[..pos]))
                .map(|&cmd| Self::Candidate { 
                    display: cmd.to_string(),
                    replacement: cmd.to_string(),
                })
                .collect();
            Ok((0, matches))
        }
    }
}

impl rustyline::Helper for PhantomCompleter {}
impl rustyline::highlight::Highlighter for PhantomCompleter {}
impl rustyline::hint::Hinter for PhantomCompleter {
    type Hint = String;
    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> { None }
}
impl rustyline::validate::Validator for PhantomCompleter {}

struct TerminalWidget {
    output: String,
    input: String,
    prompt: String,
    history: VecDeque<String>,
    history_index: Option<usize>,
    selected_text: Option<String>,
}

impl TerminalWidget {
    fn new() -> Self {
        Self {
            output: String::new(),
            input: String::new(),
            prompt: "$ ".to_string(),
            history: VecDeque::with_capacity(HISTORY_SIZE),
            history_index: None,
            selected_text: None,
        }
    }

    fn set_output(&mut self, output: &str) {
        self.output = output.to_string();
    }

    fn add_to_history(&mut self, command: String) {
        self.history.push_front(command);
        if self.history.len() > HISTORY_SIZE {
            self.history.pop_back();
        }
        self.history_index = None;
    }

    fn get_previous_command(&mut self) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }
        let index = self.history_index.map(|i| i + 1).unwrap_or(0);
        if index < self.history.len() {
            self.history_index = Some(index);
            Some(self.history[index].clone())
        } else {
            None
        }
    }

    fn get_next_command(&mut self) -> Option<String> {
        if let Some(index) = self.history_index {
            if index > 0 {
                self.history_index = Some(index - 1);
                Some(self.history[index - 1].clone())
            } else {
                self.history_index = None;
                Some(String::new())
            }
        } else {
            None
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) -> Option<String> {
        let mut executed_command = None;
    
        ui.vertical(|ui| {
            let available_size = ui.available_size();
            let output_height = available_size.y - 30.0;
    
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .max_height(output_height)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.output)
                            .font(egui::FontId::monospace(14.0))
                            .desired_width(f32::INFINITY)
                            .desired_rows((output_height / 14.0) as usize)
                            .lock_focus(true)
                            .interactive(false)
                    );
                });
    
            ui.horizontal(|ui| {
                ui.label(&self.prompt);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.input)
                        .desired_width(f32::INFINITY)
                        .font(egui::FontId::monospace(14.0))
                );
    
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let command = self.input.trim().to_string();
                    if !command.is_empty() {
                        self.add_to_history(command.clone());
                        executed_command = Some(command);
                        self.input.clear();
                    }
                    response.request_focus();
                }
    
                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    if let Some(prev_command) = self.get_previous_command() {
                        self.input = prev_command;
                    }
                }
    
                if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    if let Some(next_command) = self.get_next_command() {
                        self.input = next_command;
                    }
                }
            });
        });
    
        executed_command
    }
}

struct PhantomTTY {
    shell_path: String,
    history_file: PathBuf,
    editor: Editor<PhantomCompleter, DefaultHistory>,
    terminal: TerminalWidget,
    term: String,
    pty_master: Option<File>,
    vte_terminal: VteTerminal,
}

impl PhantomTTY {
    fn new(shell_path: String) -> Self {
        let history_file = get_history_file_path();
        let config = Config::builder()
            .history_ignore_space(true)
            .completion_type(CompletionType::List)
            .edit_mode(EditMode::Emacs)
            .build();
        let helper = PhantomCompleter {
            filename_completer: FilenameCompleter::new(),
        };
        let mut editor = Editor::with_config(config).unwrap();
        editor.set_helper(Some(helper));
        
        if let Err(err) = editor.load_history(&history_file) {
            eprintln!("Failed to load history: {}", err);
        }

        let term = match shell_path.as_str() {
            "/bin/bash" | "/usr/bin/bash" => "xterm-256color",
            "/bin/zsh" | "/usr/bin/zsh" => "xterm-256color",
            "/bin/fish" | "/usr/bin/fish" => "xterm-256color",
            _ => "vt100",
        }.to_string();

        let mut phantom_tty = PhantomTTY {
            shell_path,
            history_file,
            editor,
            terminal: TerminalWidget::new(),
            term,
            pty_master: None,
            vte_terminal: VteTerminal::new(80, 24),
        };
        phantom_tty.terminal.set_output("Welcome to PhantomTTY!\n");
        
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            phantom_tty.start_shell();
        })) {
            eprintln!("Error starting shell: {:?}", e);
            phantom_tty.terminal.set_output("Failed to start shell. Some features may not work correctly.\n");
        }
        
        phantom_tty
    }

    fn start_shell(&mut self) {
        let winsize = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let pty = openpty(Some(&winsize), None).expect("Failed to open pty");
        let pty_master = unsafe { File::from_raw_fd(pty.master) };
        let pty_slave = pty.slave;

        match unsafe { fork() }.expect("Fork failed") {
            ForkResult::Parent { child } => {
                self.pty_master = Some(pty_master);
                if let Err(e) = tcsetpgrp(pty_slave, Pid::from_raw(child.as_raw() as i32)) {
                    eprintln!("Warning: Failed to set controlling process: {}", e);
                }
            }
            ForkResult::Child => {
                if let Err(e) = setsid() {
                    eprintln!("Warning: Failed to create new session: {}", e);
                }
                
                if let Err(e) = nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)) {
                    eprintln!("Warning: Failed to set process group: {}", e);
                }
                
                if let Err(e) = tcsetpgrp(pty_slave, Pid::from_raw(0)) {
                    eprintln!("Warning: Failed to set foreground process group: {}", e);
                }
        
                unsafe {
                    if libc::ioctl(pty_slave, libc::TIOCSCTTY, 0) == -1 {
                        eprintln!("Warning: Failed to set controlling terminal");
                    }
                }

                nix::unistd::dup2(pty_slave, 0).expect("Failed to redirect stdin");
                nix::unistd::dup2(pty_slave, 1).expect("Failed to redirect stdout");
                nix::unistd::dup2(pty_slave, 2).expect("Failed to redirect stderr");

                drop(pty_master);

                let mut termios = termios::tcgetattr(pty_slave).expect("Failed to get terminal attributes");
                termios::cfmakeraw(&mut termios);
                termios::tcsetattr(pty_slave, SetArg::TCSANOW, &termios).expect("Failed to set terminal attributes");

                let err = nix::unistd::execve(
                    &std::ffi::CString::new(self.shell_path.clone()).unwrap(),
                    &[&std::ffi::CString::new(self.shell_path.clone()).unwrap()],
                    &[&std::ffi::CString::new(format!("TERM={}", self.term)).unwrap()],
                );
                panic!("Failed to execute shell: {:?}", err);
            }
        }
    }

    fn read_pty_output(&mut self) {
        if let Some(ref mut master) = self.pty_master {
            let mut fd_set = FdSet::new();
            fd_set.insert(master.as_raw_fd());
            let mut timeout = TimeVal::new(0, 100_000);
            match select(None, Some(&mut fd_set), None, None, Some(&mut timeout)) {
                Ok(_) => {
                    if fd_set.contains(master.as_raw_fd()) {
                        let mut buffer = [0u8; 1024];
                        match master.read(&mut buffer) {
                            Ok(n) if n > 0 => {
                                self.vte_terminal.process(&buffer[..n]);
                                self.terminal.set_output(&self.vte_terminal.get_screen());
                            }
                            Err(e) => eprintln!("Error reading from PTY: {}", e),
                            _ => {}
                        }
                    }
                }
                Err(e) => eprintln!("Error in select: {}", e),
            }
        }
    }
    fn save_history(&mut self) {
        if let Err(err) = self.editor.save_history(&self.history_file) {
            eprintln!("Error saving history: {}", err);
        }
    }

    fn execute_command(&mut self, command: &str) -> io::Result<()> {
        self.editor.add_history_entry(command.to_string()).unwrap();

        match command {
            "history" => self.show_history(),
            "exit" => {
                self.save_history();
                std::process::exit(0);
            },
            _ if command.starts_with("phantom:") => self.handle_phantom_command(&command[8..]),
            _ => self.execute_in_shell(command),
        }
    }

    fn show_history(&mut self) -> io::Result<()> {
        let history_output: String = self.terminal.history
            .iter()
            .enumerate()
            .map(|(i, cmd)| format!("{}: {}\n", i + 1, cmd))
            .collect();
        self.terminal.set_output(&history_output);
        Ok(())
    }

    fn handle_phantom_command(&mut self, command: &str) -> io::Result<()> {
        match command.trim() {
            "hello" => self.terminal.set_output("Hello from PhantomTTY!"),
            "shell" => self.terminal.set_output(&format!("Current shell: {}", self.shell_path)),
            _ => self.terminal.set_output(&format!("Unknown PhantomTTY command: {}", command)),
        }
        Ok(())
    }

    fn execute_in_shell(&mut self, command: &str) -> io::Result<()> {
        if let Some(ref mut master) = self.pty_master {
            writeln!(master, "{}", command)?;
            master.flush()?;
        }
        Ok(())
    }
}

struct PhantomTTYApp {
    phantom_tty: PhantomTTY,
}

impl PhantomTTYApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let shell_path = get_default_shell();
        Self {
            phantom_tty: PhantomTTY::new(shell_path),
        }
    }
}

impl eframe::App for PhantomTTYApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.phantom_tty.read_pty_output();

        egui::CentralPanel::default().show(ctx, |ui| {
            let available_size = ui.available_size();
            
            if let Some(command) = self.phantom_tty.terminal.show(ui, ctx) {
                if let Err(e) = self.phantom_tty.execute_command(&command) {
                    self.phantom_tty.terminal.set_output(&format!("Error: {}", e));
                }
            }
        });

        ctx.request_repaint();
    }
}

fn get_history_file_path() -> PathBuf {
    let mut path = if let Some(config_dir) = dirs::config_dir() {
        config_dir
    } else {
        PathBuf::from(env::var("HOME").unwrap_or_else(|_| String::from("/")))
    };
    path.push("phantomtty");
    fs::create_dir_all(&path).unwrap_or_else(|e| eprintln!("Error creating config directory: {}", e));
    path.push("history");
    path
}

fn get_default_shell() -> String {
    if let Ok(shell) = env::var("SHELL") {
        return shell;
    }

    let username = env::var("USER").unwrap_or_else(|_| String::from("root"));
    if let Ok(file) = File::open("/etc/passwd") {
        let reader = BufReader::new(file);
        for line in reader.lines() {
            if let Ok(line) = line {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 7 && parts[0] == username {
                    return String::from(parts[6]);
                }
            }
        }
    }

    String::from("/bin/sh")
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        initial_window_size: Some(egui::vec2(800.0, 600.0)),
        ..Default::default()
    };
    eframe::run_native(
        "PhantomTTY",
        options,
        Box::new(|cc| Ok(Box::new(PhantomTTYApp::new(cc)))),
    )
}