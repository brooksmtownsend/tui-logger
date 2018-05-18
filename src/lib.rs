extern crate chrono;
extern crate termion;
extern crate tui;
#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;
extern crate fxhash;
extern crate parking_lot;

use std::cell::RefCell;
use std::collections::hash_map::Iter;
use std::collections::hash_map::Keys;
use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::mem;
use std::rc::Rc;

use chrono::{DateTime, Local};
use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;
use termion::event::*;
use tui::buffer::Buffer;
use tui::backend::Backend;
use tui::Terminal;
use tui::layout::{Direction, Group, Rect, Size};
use tui::style::{Modifier, Style};
use tui::widgets::{Block, Borders, Widget};

mod circular;
mod dispatcher;

pub use circular::CircularBuffer;
pub use dispatcher::{Dispatcher, EventListener};

struct ExtLogRecord {
    timestamp: DateTime<Local>,
    level: Level,
    target: String,
    file: String,
    line: u32,
    msg: String,
}

fn advance_levelfilter(levelfilter: &LevelFilter) -> (LevelFilter, LevelFilter) {
    match levelfilter {
        &LevelFilter::Trace => (LevelFilter::Trace, LevelFilter::Debug),
        &LevelFilter::Debug => (LevelFilter::Trace, LevelFilter::Info),
        &LevelFilter::Info => (LevelFilter::Debug, LevelFilter::Warn),
        &LevelFilter::Warn => (LevelFilter::Info, LevelFilter::Error),
        &LevelFilter::Error => (LevelFilter::Warn, LevelFilter::Off),
        &LevelFilter::Off => (LevelFilter::Error, LevelFilter::Off),
    }
}

pub struct LevelConfig {
    config: HashMap<String, LevelFilter>,
    generation: u64,
    origin_generation: u64
}
impl LevelConfig {
    pub fn new() -> LevelConfig {
        LevelConfig {
            config: HashMap::new(),
            generation: 1,
            origin_generation: 0
        }
    }
    pub fn set(&mut self, target: &str, level: LevelFilter) {
        if let Some(lev) = self.config.get_mut(target) {
            if *lev != level {
                *lev = level;
                self.generation += 1;
            }
            return;
        }
        self.config.insert(target.to_string(), level);
        self.generation += 1;
    }
    pub fn keys(&self) -> Keys<String, LevelFilter> {
        self.config.keys()
    }
    pub fn get(&self, target: &str) -> Option<&LevelFilter> {
        self.config.get(target)
    }
    pub fn iter(&self) -> Iter<String, LevelFilter> {
        self.config.iter()
    }
    fn merge(&mut self, origin: &LevelConfig) {
        if self.origin_generation != origin.generation {
            for (target, origin_levelfilter) in origin.iter() {
                if let Some(levelfilter) = self.get(target) {
                    if levelfilter <= origin_levelfilter {
                        continue;
                    }
                }
                self.set(target, *origin_levelfilter);
            }
            self.generation = origin.generation;
        }
    }
}
struct HotSelect {
    hashtable: Vec<(Option<u64>, LevelFilter)>,
    default: LevelFilter,
}
struct HotLog {
    events: CircularBuffer<ExtLogRecord>,
}
struct TuiLoggerInner {
    hot_depth: usize,
    events: CircularBuffer<ExtLogRecord>,
    total_events: usize,
    dump: Option<File>,
    default: LevelFilter,
    targets: LevelConfig,
}
struct TuiLogger {
    hot_select: Mutex<HotSelect>,
    hot_log: Mutex<HotLog>,
    inner: Mutex<TuiLoggerInner>,
}
impl TuiLogger {
    pub fn move_events(&self) {
        // If there are no new events, then just return
        if self.hot_log.lock().events.total_elements() == 0 {
            return;
        }
        // Exchange new event buffer with the hot buffer
        let mut received_events = {
            let new_circular = CircularBuffer::new(self.inner.lock().hot_depth);
            let mut hl = self.hot_log.lock();
            mem::replace(&mut hl.events, new_circular)
        };
        let mut tli = self.inner.lock();
        tli.total_events += received_events.total_elements();
        let has_wrapped = received_events.has_wrapped();
        let mut consumed = received_events.take();
        let mut reversed = Vec::with_capacity(consumed.len() + 1);
        while let Some(log_entry) = consumed.pop() {
            reversed.push(log_entry);
        }
        if has_wrapped {
            // Too many events received, so some have been lost
            let log_entry = ExtLogRecord {
                timestamp: chrono::Local::now(),
                level: Level::Warn,
                target: "TuiLogger".to_string(),
                file: "?".to_string(),
                line: 0,
                msg: format!("There have been lost some logging events"),
            };
            reversed.push(log_entry);
        }
        let default_level = tli.default;
        while let Some(log_entry) = reversed.pop() {
            if tli.targets.get(&log_entry.target).is_none() {
                tli.targets.set(&log_entry.target, default_level);
            }
            if let Some(ref mut file) = tli.dump {
                if let Err(_e) = writeln!(
                    file,
                    "{}:{}:{}:{}:{}",
                    &log_entry.timestamp.format("[%Y:%m:%d %H:%M:%S]"),
                    log_entry.level,
                    &log_entry.file,
                    log_entry.line,
                    &log_entry.msg
                ) {
                    // TODO: What to do in case of write error ?
                }
            }
            tli.events.push(log_entry);
        }
    }
}
lazy_static! {
    static ref TUI_LOGGER: TuiLogger = {
        let hs = HotSelect {
            hashtable: vec![],
            default: LevelFilter::Info,
        };
        let hl = HotLog {
            events: CircularBuffer::new(1000),
        };
        let tli = TuiLoggerInner {
            hot_depth: 1000,
            events: CircularBuffer::new(10000),
            total_events: 0,
            dump: None,
            default: LevelFilter::Info,
            targets: LevelConfig::new(),
        };
        TuiLogger {
            hot_select: Mutex::new(hs),
            hot_log: Mutex::new(hl),
            inner: Mutex::new(tli),
        }
    };
}
pub fn init_logger(max_level: LevelFilter) -> Result<(), log::SetLoggerError> {
    for _ in 0..1027 {
        TUI_LOGGER
            .hot_select
            .lock()
            .hashtable
            .push((None, max_level));
    }
    log::set_max_level(max_level);
    log::set_logger(&*TUI_LOGGER)
}
pub fn set_log_file(fname: String) {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fname)
        .unwrap();
    TUI_LOGGER.inner.lock().dump = Some(file);
}
pub fn set_default_level(levelfilter: LevelFilter) {
    TUI_LOGGER.hot_select.lock().default = levelfilter;
    TUI_LOGGER.inner.lock().default = levelfilter;
}
pub fn set_level_for_target(target: &str, levelfilter: LevelFilter) {
    let h = fxhash::hash64(&target);
    TUI_LOGGER.inner.lock().targets.set(target, levelfilter);
    let mut hs = TUI_LOGGER.hot_select.lock();
    let hl = hs.hashtable.len() as u64;
    hs.hashtable[(h % hl) as usize] = (Some(h), levelfilter);
}
impl Log for TuiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let h = fxhash::hash64(metadata.target());
        let hs = self.hot_select.lock();
        let hl = hs.hashtable.len() as u64;
        let (opt_hash, levelfilter) = hs.hashtable[(h % hl) as usize];
        if Some(h) == opt_hash {
            metadata.level() <= levelfilter
        } else {
            metadata.level() <= hs.default
        }
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let log_entry = ExtLogRecord {
                timestamp: chrono::Local::now(),
                level: record.level(),
                target: record.target().to_string(),
                file: record.file().unwrap_or("?").to_string(),
                line: record.line().unwrap_or(0),
                msg: format!("{}", record.args()),
            };
            self.hot_log.lock().events.push(log_entry);
        }
    }

    fn flush(&self) {}
}


struct TuiWidgetInnerState {
    config: LevelConfig,
    selected: Option<usize>,
    offset: usize,
    hide_off: bool,
    hide_target: bool
}
impl TuiWidgetInnerState {
    pub fn new() -> TuiWidgetInnerState {
        TuiWidgetInnerState {
            config: LevelConfig::new(),
            selected: None,
            offset: 0,
            hide_off: false,
            hide_target: false
        }
    }
}
pub struct TuiWidgetState {
    inner: Rc<RefCell<TuiWidgetInnerState>>,
}
impl TuiWidgetState {
    pub fn new() -> TuiWidgetState {
        TuiWidgetState {
            inner: Rc::new(RefCell::new(TuiWidgetInnerState::new())),
        }
    }
}
pub struct TuiLoggerTargetWidget<'b> {
    block: Option<Block<'b>>,
    /// Base style of the widget
    style: Style,
    style_show: Style,
    style_hide: Style,
    style_off: Option<Style>,
    highlight_style: Style,
    state: Rc<RefCell<TuiWidgetInnerState>>,
    targets: Vec<String>,
    event_dispatcher: Option<Rc<RefCell<Dispatcher<Event>>>>,
}
impl<'b> Default for TuiLoggerTargetWidget<'b> {
    fn default() -> TuiLoggerTargetWidget<'b> {
        TUI_LOGGER.move_events();
        TuiLoggerTargetWidget {
            block: None,
            style: Default::default(),
            style_off: None,
            style_hide: Style::default(),
            style_show: Style::default().modifier(Modifier::Invert),
            highlight_style: Style::default().modifier(Modifier::Invert),
            state: Rc::new(RefCell::new(TuiWidgetInnerState::new())),
            targets: vec![],
            event_dispatcher: None,
        }
    }
}
impl<'b> TuiLoggerTargetWidget<'b> {
    pub fn block(&'b mut self, block: Block<'b>) -> &mut TuiLoggerTargetWidget<'b> {
        self.block = Some(block);
        self
    }
    fn opt_style(&'b mut self, style: Option<Style>) -> &mut TuiLoggerTargetWidget<'b> {
        if let Some(s) = style {
            self.style = s;
        }
        self
    }
    fn opt_style_off(&'b mut self, style: Option<Style>) -> &mut TuiLoggerTargetWidget<'b> {
        if style.is_some() {
            self.style_off = style;
        }
        self
    }
    fn opt_style_hide(&'b mut self, style: Option<Style>) -> &mut TuiLoggerTargetWidget<'b> {
        if let Some(s) = style {
            self.style_hide = s;
        }
        self
    }
    fn opt_style_show(&'b mut self, style: Option<Style>) -> &mut TuiLoggerTargetWidget<'b> {
        if let Some(s) = style {
            self.style_show = s;
        }
        self
    }
    fn opt_highlight_style(&'b mut self, style: Option<Style>) -> &mut TuiLoggerTargetWidget<'b> {
        if let Some(s) = style {
            self.highlight_style = s;
        }
        self
    }
    pub fn style(&'b mut self, style: Style) -> &mut TuiLoggerTargetWidget<'b> {
        self.style = style;
        self
    }
    pub fn style_off(&'b mut self, style: Style) -> &mut TuiLoggerTargetWidget<'b> {
        self.style_off = Some(style);
        self
    }
    pub fn style_hide(&'b mut self, style: Style) -> &mut TuiLoggerTargetWidget<'b> {
        self.style_hide = style;
        self
    }
    pub fn style_show(&'b mut self, style: Style) -> &mut TuiLoggerTargetWidget<'b> {
        self.style_show = style;
        self
    }
    pub fn highlight_style(&'b mut self, style: Style) -> &mut TuiLoggerTargetWidget<'b> {
        self.highlight_style = style;
        self
    }
    fn inner_state(&'b mut self, state: Rc<RefCell<TuiWidgetInnerState>>) -> &mut TuiLoggerTargetWidget<'b> {
        self.state = state.clone();
        self
    }
    pub fn state(&'b mut self, state: &TuiWidgetState) -> &mut TuiLoggerTargetWidget<'b> {
        self.state = state.inner.clone();
        self
    }
    fn opt_dispatcher(
        &mut self,
        dispatcher: Option<Rc<RefCell<Dispatcher<Event>>>>,
    ) -> &mut TuiLoggerTargetWidget<'b> {
        if let Some(d) = dispatcher {
            self.event_dispatcher = Some(d.clone());
        }
        self
    }
    fn add_to_dispatcher(&mut self) {
        if let Some(ref dispatcher) = self.event_dispatcher {
            let state = self.state.clone();
            if state.borrow().hide_off {
                dispatcher.borrow_mut().add_listener(move |evt| {
                    if &Event::Key(Key::Char(' ')) == evt {
                        state.borrow_mut().hide_off = false;
                        true
                    } else {
                        false
                    }
                });
            } else {
                dispatcher.borrow_mut().add_listener(move |evt| {
                    if &Event::Key(Key::Char(' ')) == evt {
                        state.borrow_mut().hide_off = true;
                        true
                    } else {
                        false
                    }
                });
            }
            if self.targets.len() > 0 {
                let state = self.state.clone();
                if self.state.borrow().selected.is_none() {
                    dispatcher.borrow_mut().add_listener(move |evt| {
                        if &Event::Key(Key::Down) == evt || &Event::Key(Key::Up) == evt {
                            state.borrow_mut().selected = Some(0);
                            true
                        } else {
                            false
                        }
                    });
                } else {
                    let selected = self.state.borrow().selected.unwrap();
                    let max_selected = self.targets.len();
                    if selected > 0 {
                        let state = state.clone();
                        dispatcher.borrow_mut().add_listener(move |evt| {
                            if &Event::Key(Key::Up) == evt {
                                state.borrow_mut().selected = Some(selected - 1);
                                true
                            } else {
                                false
                            }
                        })
                    }
                    if selected + 1 < max_selected {
                        let state = self.state.clone();
                        dispatcher.borrow_mut().add_listener(move |evt| {
                            if &Event::Key(Key::Down) == evt {
                                state.borrow_mut().selected = Some(selected + 1);
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
                if self.state.borrow().selected.is_some() {
                    let selected = self.state.borrow().selected.unwrap();
                    let t = self.targets[selected].clone();
                    let (more, less) = if let Some(levelfilter) = self.state.borrow().config.get(&t)
                    {
                        advance_levelfilter(levelfilter)
                    } else {
                        return;
                    };
                    let state = self.state.clone();
                    dispatcher.borrow_mut().add_listener(move |evt| {
                        if &Event::Key(Key::Left) == evt {
                            state.borrow_mut().config.set(&t, less);
                            true
                        } else if &Event::Key(Key::Right) == evt {
                            state.borrow_mut().config.set(&t, more);
                            true
                        } else {
                            false
                        }
                    });
                    let t = self.targets[selected].clone();
                    let (more, less) =
                        if let Some(levelfilter) = TUI_LOGGER.inner.lock().targets.get(&t) {
                            advance_levelfilter(levelfilter)
                        } else {
                            return;
                        };
                    dispatcher.borrow_mut().add_listener(move |evt| {
                        if &Event::Key(Key::Char('-')) == evt {
                            set_level_for_target(&t, less);
                            true
                        } else if &Event::Key(Key::Char('+')) == evt {
                            set_level_for_target(&t, more);
                            true
                        } else {
                            false
                        }
                    });
                }
            }
        };
    }
}
impl<'b> Widget for TuiLoggerTargetWidget<'b> {
    fn draw(&mut self, area: &Rect, buf: &mut Buffer) {
        let list_area = match self.block {
            Some(ref mut b) => {
                b.draw(area, buf);
                b.inner(area)
            }
            None => *area,
        };
        if list_area.width < 8 || list_area.height < 1 {
            return;
        }
        self.background(&list_area, buf, self.style.bg);

        let la_left = list_area.left();
        let la_top = list_area.top();
        let la_width = list_area.width as usize;

        {
            let hot_targets = &TUI_LOGGER.inner.lock().targets;
            let mut state = self.state.borrow_mut();
            let mut selected = state.selected;
            let hide_off = state.hide_off;
            let offset = state.offset;
            {
                let ref mut targets = &mut state.config;
                targets.merge(hot_targets);
                self.targets.clear();
                for (t,levelfilter) in targets.iter() {
                    if hide_off {
                        if levelfilter == &LevelFilter::Off {
                            continue;
                        }
                    }
                    self.targets.push(t.clone());
                }
                self.targets.sort();
            }
            if let Some(sel) = selected {
                if sel >= self.targets.len() {
                    state.selected = None;
                    selected = None;
                }
            }
            let list_height = (list_area.height as usize).min(self.targets.len());
            let offset = if list_height > self.targets.len() {
                    0
                }
                else {
                    if let Some(sel) = selected {
                        if sel >= offset+list_height {
                            sel - list_height + 1
                        }
                        else if sel <= offset {
                            sel
                        }
                        else {
                            offset
                        }
                    }
                    else {
                         0
                    }
                };
            state.offset = offset;

            let ref targets = &state.config;
            for i in 0..list_height {
                let t = &self.targets[i+offset];
                let hot_level_filter = hot_targets.get(&t).unwrap();
                let level_filter = targets.get(&t).unwrap();
                for (j, sym, lev) in vec![
                    (0, "E", Level::Error),
                    (1, "W", Level::Warn),
                    (2, "I", Level::Info),
                    (3, "D", Level::Debug),
                    (4, "T", Level::Trace),
                ] {
                    let mut cell = buf.get_mut(la_left + j, la_top + i as u16);
                    cell.style = if *hot_level_filter >= lev {
                        if *level_filter >= lev {
                            self.style_show
                        } else {
                            self.style_hide
                        }
                    } else {
                        if let Some(style_off) = self.style_off {
                            style_off
                        } else {
                            cell.symbol = " ".to_string();
                            continue;
                        }
                    };
                    cell.symbol = sym.to_string();
                }
                buf.set_stringn(la_left + 5, la_top + i as u16, &":", la_width, &self.style);
                buf.set_stringn(
                    la_left + 6,
                    la_top + i as u16,
                    t,
                    la_width,
                    if Some(i) == selected {
                        &self.highlight_style
                    } else {
                        &self.style
                    },
                );
            }
        }
        self.add_to_dispatcher();
    }
}
impl<'b> EventListener<Event> for TuiLoggerTargetWidget<'b> {
    fn dispatcher(
        &mut self,
        dispatcher: Rc<RefCell<Dispatcher<Event>>>,
    ) -> &mut TuiLoggerTargetWidget<'b> {
        self.event_dispatcher = Some(dispatcher.clone());
        self
    }
}

pub struct TuiLoggerWidget<'b> {
    block: Option<Block<'b>>,
    /// Base style of the widget
    style: Style,
    /// Level based style
    style_error: Option<Style>,
    style_warn: Option<Style>,
    style_debug: Option<Style>,
    style_trace: Option<Style>,
    style_info: Option<Style>,
    state: Rc<RefCell<TuiWidgetInnerState>>,
}
impl<'b> Default for TuiLoggerWidget<'b> {
    fn default() -> TuiLoggerWidget<'b> {
        TUI_LOGGER.move_events();
        TuiLoggerWidget {
            block: None,
            style: Default::default(),
            style_error: None,
            style_warn: None,
            style_debug: None,
            style_trace: None,
            style_info: None,
            state: Rc::new(RefCell::new(TuiWidgetInnerState::new())),
        }
    }
}
impl<'b> TuiLoggerWidget<'b> {
    pub fn block(&'b mut self, block: Block<'b>) -> &mut TuiLoggerWidget<'b> {
        self.block = Some(block);
        self
    }
    fn opt_style(&'b mut self, style: Option<Style>) -> &mut TuiLoggerWidget<'b> {
        if let Some(s) = style {
            self.style = s;
        }
        self
    }
    fn opt_style_error(&'b mut self, style: Option<Style>) -> &mut TuiLoggerWidget<'b> {
        if style.is_some() {
            self.style_error = style;
        }
        self
    }
    fn opt_style_warn(&'b mut self, style: Option<Style>) -> &mut TuiLoggerWidget<'b> {
        if style.is_some() {
            self.style_warn = style;
        }
        self
    }
    fn opt_style_info(&'b mut self, style: Option<Style>) -> &mut TuiLoggerWidget<'b> {
        if style.is_some() {
            self.style_info = style;
        }
        self
    }
    fn opt_style_trace(&'b mut self, style: Option<Style>) -> &mut TuiLoggerWidget<'b> {
        if style.is_some() {
            self.style_trace = style;
        }
        self
    }
    fn opt_style_debug(&'b mut self, style: Option<Style>) -> &mut TuiLoggerWidget<'b> {
        if style.is_some() {
            self.style_debug = style;
        }
        self
    }
    pub fn style(&'b mut self, style: Style) -> &mut TuiLoggerWidget<'b> {
        self.style = style;
        self
    }
    pub fn style_error(&'b mut self, style: Style) -> &mut TuiLoggerWidget<'b> {
        self.style_error = Some(style);
        self
    }
    pub fn style_warn(&'b mut self, style: Style) -> &mut TuiLoggerWidget<'b> {
        self.style_warn = Some(style);
        self
    }
    pub fn style_info(&'b mut self, style: Style) -> &mut TuiLoggerWidget<'b> {
        self.style_info = Some(style);
        self
    }
    pub fn style_trace(&'b mut self, style: Style) -> &mut TuiLoggerWidget<'b> {
        self.style_trace = Some(style);
        self
    }
    pub fn style_debug(&'b mut self, style: Style) -> &mut TuiLoggerWidget<'b> {
        self.style_debug = Some(style);
        self
    }
    fn inner_state(&'b mut self, state: Rc<RefCell<TuiWidgetInnerState>>) -> &mut TuiLoggerWidget<'b> {
        self.state = state.clone();
        self
    }
    pub fn state(&'b mut self, state: &TuiWidgetState) -> &mut TuiLoggerWidget<'b> {
        self.state = state.inner.clone();
        self
    }
}
impl<'b> Widget for TuiLoggerWidget<'b> {
    fn draw(&mut self, area: &Rect, buf: &mut Buffer) {
        let list_area = match self.block {
            Some(ref mut b) => {
                b.draw(area, buf);
                b.inner(area)
            }
            None => *area,
        };
        if list_area.width < 8 || list_area.height < 1 {
            return;
        }
        self.background(&list_area, buf, self.style.bg);

        let state = self.state.borrow();
        let list_height = list_area.height as usize;
        let mut lines = vec![];
        {
            let mut tui_lock = TUI_LOGGER.inner.lock();
            for l in tui_lock.events.rev_iter() {
                if let Some(level) = state.config.get(&l.target) {
                    if *level < l.level {
                        continue;
                    }
                }
                let mut output = String::new();
                output.push_str(&format!("{}", l.timestamp.format("%H:%M:%S")));
                output.push(':');
                let (col_style, txt, with_loc) = match l.level {
                    log::Level::Error => (self.style_error, "ERROR", true),
                    log::Level::Warn => (self.style_warn, "WARN ", true),
                    log::Level::Info => (self.style_info, "INFO ", false),
                    log::Level::Debug => (self.style_debug, "DEBUG", true),
                    log::Level::Trace => (self.style_trace, "TRACE", true),
                };
                output.push_str(txt);
                if with_loc {
                    output.push(':');
                    output.push_str(&l.file);
                    output.push(':');
                    output.push_str(&format!("{}", l.line));
                }
                output.push(':');
                let mut x_iter = l.msg.lines();
                output.push_str(x_iter.next().unwrap());
                for sublines in x_iter.rev() {
                    lines.push((col_style, sublines.to_string()));
                }
                lines.push((col_style, output));
                if lines.len() == list_height {
                    break;
                }
            }
        }
        let la_left = list_area.left();
        let la_top = list_area.top();
        let la_width = list_area.width as usize;
        let mut i = 0;
        loop {
            if let Some((sty, l)) = lines.pop() {
                let cp = l.len() as isize;
                let nr_lines = ((cp - 7) / ((la_width - 7) as isize)) as usize + 1;
                let mut left;
                let mut l_from: usize;
                let mut l_to: usize;
                for j in (0..nr_lines).rev() {
                    if j == 0 {
                        left = 0;
                        l_from = 0;
                        l_to = l.len().min(la_width);
                    } else {
                        left = 7;
                        l_from = la_width + (j - 1) * (la_width - 7);
                        l_to = (la_width + j * (la_width - 7)).min(l.len());
                    }
                    buf.set_stringn(
                        la_left + left,
                        la_top + i as u16,
                        &l[l_from..l_to],
                        l_to - l_from,
                        &sty.unwrap_or(self.style),
                    );
                    i = i + 1;
                    if i == list_height {
                        return;
                    }
                }
            } else {
                return;
            }
        }
    }
}

pub struct TuiLoggerSmartWidget<'b> {
    title_log: String,
    title_target: String,
    block: Option<Block<'b>>,
    style: Option<Style>,
    border_style: Style,
    highlight_style: Option<Style>,
    style_error: Option<Style>,
    style_warn: Option<Style>,
    style_debug: Option<Style>,
    style_trace: Option<Style>,
    style_info: Option<Style>,
    style_show: Option<Style>,
    style_hide: Option<Style>,
    style_off: Option<Style>,
    state: Rc<RefCell<TuiWidgetInnerState>>,
    event_dispatcher: Option<Rc<RefCell<Dispatcher<Event>>>>,
}
impl<'b> Default for TuiLoggerSmartWidget<'b> {
    fn default() -> TuiLoggerSmartWidget<'b> {
        TUI_LOGGER.move_events();
        TuiLoggerSmartWidget {
            title_log: "Tui Log".to_owned(),
            title_target: "Tui Target Selector".to_owned(),
            block: None,
            style: None,
            border_style: Style::default(),
            highlight_style: None,
            style_error: None,
            style_warn: None,
            style_debug: None,
            style_trace: None,
            style_info: None,
            style_show: None,
            style_hide: None,
            style_off: None,
            state: Rc::new(RefCell::new(TuiWidgetInnerState::new())),
            event_dispatcher: None,
        }
    }
}
impl<'b> TuiLoggerSmartWidget<'b> {
    pub fn block(&'b mut self, block: Block<'b>) -> &mut TuiLoggerSmartWidget<'b> {
        self.block = Some(block);
        self
    }
    pub fn highlight_style(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.highlight_style = Some(style);
        self
    }
    pub fn border_style(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.border_style = style;
        self
    }
    pub fn style(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style = Some(style);
        self
    }
    pub fn style_error(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_error = Some(style);
        self
    }
    pub fn style_warn(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_warn = Some(style);
        self
    }
    pub fn style_info(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_info = Some(style);
        self
    }
    pub fn style_trace(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_trace = Some(style);
        self
    }
    pub fn style_debug(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_debug = Some(style);
        self
    }
    pub fn style_off(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_off = Some(style);
        self
    }
    pub fn style_hide(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_hide = Some(style);
        self
    }
    pub fn style_show(&'b mut self, style: Style) -> &mut TuiLoggerSmartWidget<'b> {
        self.style_show = Some(style);
        self
    }
    pub fn state(&'b mut self, state: &TuiWidgetState) -> &mut TuiLoggerSmartWidget<'b> {
        self.state = state.inner.clone();
        self
    }
}
impl<'b> EventListener<Event> for TuiLoggerSmartWidget<'b> {
    fn dispatcher(
        &mut self,
        dispatcher: Rc<RefCell<Dispatcher<Event>>>,
    ) -> &mut TuiLoggerSmartWidget<'b> {
        self.event_dispatcher = Some(dispatcher.clone());
        self
    }
}
impl<'b> Widget for TuiLoggerSmartWidget<'b> {
    /// Nothing to draw for combo widget
    fn draw(&mut self, _area: &Rect, _buf: &mut Buffer) {
    }
    fn render<B>(&mut self, t: &mut Terminal<B>, area: &Rect) 
        where
            Self: Sized,
            B: Backend, {
        let hide_target = self.state.borrow().hide_target;
        if let Some(ref dispatcher) = self.event_dispatcher {
            let state = self.state.clone();
            if hide_target {
                dispatcher.borrow_mut().add_listener(move |evt| {
                    if &Event::Key(Key::Char('h')) == evt {
                        state.borrow_mut().hide_target = false;
                        true
                    } else {
                        false
                    }
                });
            } else {
                dispatcher.borrow_mut().add_listener(move |evt| {
                    if &Event::Key(Key::Char('h')) == evt {
                        state.borrow_mut().hide_target = true;
                        true
                    } else {
                        false
                    }
                });
            }
        }
        if hide_target {
            TuiLoggerWidget::default()
                .block(
                    Block::default()
                        .title(&self.title_log)
                        .border_style(self.border_style)
                        .borders(Borders::ALL),
                )
                .opt_style(self.style)
                .opt_style_error(self.style_error)
                .opt_style_warn(self.style_warn)
                .opt_style_info(self.style_info)
                .opt_style_debug(self.style_debug)
                .opt_style_trace(self.style_trace)
                .inner_state(self.state.clone())
                .render(t, &area);
        }
        else {
            let mut width: usize = 0;
            {
                let hot_targets = &TUI_LOGGER.inner.lock().targets;
                let mut state = self.state.borrow_mut();
                let hide_off = state.hide_off;
                {
                    let ref mut targets = &mut state.config;
                    targets.merge(hot_targets);
                    for (t,levelfilter) in targets.iter() {
                        if hide_off {
                            if levelfilter == &LevelFilter::Off {
                                continue;
                            }
                        }
                        width = width.max(t.len())
                    }
                }
            }
            Group::default()
                .direction(Direction::Horizontal)
                .sizes(&[Size::Fixed(width as u16+6+2), Size::Min(10)])
                .render(t, area, |t, chunks| {
                    TuiLoggerTargetWidget::default()
                        .block(
                            Block::default()
                                .title(&self.title_target)
                                .border_style(self.border_style)
                                .borders(Borders::ALL)
                        )
                        .opt_style(self.style)
                        .opt_highlight_style(self.highlight_style)
                        .opt_style_off(self.style_off)
                        .opt_style_hide(self.style_hide)
                        .opt_style_show(self.style_show)
                        .inner_state(self.state.clone())
                        .opt_dispatcher(self.event_dispatcher.take())
                        .render(t, &chunks[0]);
                    TuiLoggerWidget::default()
                        .block(
                            Block::default()
                                .title(&self.title_log)
                                .border_style(self.border_style)
                                .borders(Borders::ALL),
                        )
                        .opt_style(self.style)
                        .opt_style_error(self.style_error)
                        .opt_style_warn(self.style_warn)
                        .opt_style_info(self.style_info)
                        .opt_style_debug(self.style_debug)
                        .opt_style_trace(self.style_trace)
                        .inner_state(self.state.clone())
                        .render(t, &chunks[1]);
                });
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn test() {
        assert_eq!(1 + 1, 2);
    }
}
