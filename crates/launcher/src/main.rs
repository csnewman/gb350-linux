//! The GB350's game launcher: browse the SD card's games by system, search
//! them, change settings, and start them in the emulator (`retro`), which
//! takes over the display until the game is quit (hold Select + Start).
//!
//! Started by the agent (PID 1) at boot, which hands it the display and
//! restarts it if it crashes. Usage: `launcher [CARD_ROOT]` (default
//! `/mnt/sdcard`); its files go in `CARD_ROOT/gb350/launcher/`.

mod app;
mod library;
mod preview;
mod remote;
mod settings;
mod view;

use app::{Action, App};
use gb350_ui::display::Display;
use gb350_ui::input::{self, Event, Input};
use library::{Library, SYSTEMS};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--snapshot") {
        let [_, root, out, script] = &args[..] else {
            eprintln!("usage: launcher --snapshot CARD_ROOT OUT_DIR 'key,key,shot:NAME,type:TEXT,...'");
            std::process::exit(2);
        };
        if let Err(e) = snapshot(Path::new(root), Path::new(out), script) {
            eprintln!("launcher: {e}");
            std::process::exit(1);
        }
        return;
    }
    let root = PathBuf::from(args.first().cloned().unwrap_or_else(|| "/mnt/sdcard".into()));
    match run(&root) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("launcher: {e}");
            std::process::exit(1);
        }
    }
}

fn run(root: &Path) -> Result<i32, String> {
    let t0 = Instant::now();
    let _ = std::fs::create_dir_all(Library::data_dir(root));
    let mut display = Display::open()?;
    let t_display = t0.elapsed().as_millis();
    let mut ui = gb350_ui::Ui::new();
    let t_fonts = t0.elapsed().as_millis();
    let settings = settings::Settings::load(Library::data_dir(root).join("settings.conf"));
    settings.apply_all();
    let lib = Library::scan(root);
    eprintln!(
        "launcher: {} games in {} systems; ready after {} ms (display {t_display}, fonts {t_fonts})",
        lib.total(),
        lib.systems_used(),
        t0.elapsed().as_millis()
    );
    let previews = preview::Previews::new(view::PREVIEW_W, view::PREVIEW_H);
    let mut input = Input::open();
    let mut app = App::new(lib, settings, previews);
    app.headphones = input.headphones;
    // The emulators live on the card (the launcher may run from a copy in RAM).
    let bin_dir = root.join("gb350/bin");

    let mut remote = remote::Remote::open();
    let mut dirty = true;
    let mut events = vec![];
    loop {
        if dirty && !display.busy() {
            let t = Instant::now();
            display.draw(&mut ui, |c| view::draw(c, &mut app));
            dirty = false;
            let ms = t.elapsed().as_millis();
            if ms > 25 {
                eprintln!("launcher: slow frame: {ms} ms");
            }
        }
        // Wait for buttons, a finished flip, a loaded screenshot or a timer.
        let now = Instant::now();
        let mut deadline = app.deadline();
        if let Some(d) = input.deadline() {
            deadline = deadline.min(d);
        }
        let timeout = deadline.saturating_duration_since(now).as_millis().min(60_000) as i32;
        let mut fds: Vec<libc::pollfd> = input.fds().into_iter().map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        fds.push(libc::pollfd { fd: display.fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: app.previews.fd(), events: libc::POLLIN, revents: 0 });
        if let Some(r) = &remote {
            fds.push(libc::pollfd { fd: r.fd(), events: libc::POLLIN, revents: 0 });
        }
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        let n = input.fds().len();
        if fds[n].revents != 0 {
            display.flip_done();
        }
        if fds[n + 1].revents != 0 && app.previews.collect() {
            dirty = true;
        }

        events.clear();
        if let Some(r) = &mut remote {
            for line in r.lines() {
                match line.split_once(' ') {
                    Some(("key", k)) => match input::Key::parse(k) {
                        Some(k) => events.push(Event::Press(k)),
                        None => eprintln!("launcher: remote: unknown key {k}"),
                    },
                    // `launch <path on the card>[|KEY=value ...]`: a game through
                    // the normal path, with extra emulator settings (tests).
                    Some(("launch", rest)) => {
                        let (path, env) = rest.split_once('|').unwrap_or((rest, ""));
                        let env: Vec<(String, String)> =
                            env.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        match app.lib.find(path.trim()) {
                            Some(g) => {
                                launch(&mut app, &mut display, &mut ui, &mut input, &bin_dir, g, &env);
                                dirty = true;
                            }
                            None => eprintln!("launcher: remote: no game {path:?}"),
                        }
                    }
                    Some(("shot", path)) => {
                        // Finish the frame first.
                        if dirty {
                            display.settle();
                            display.draw(&mut ui, |c| view::draw(c, &mut app));
                            dirty = false;
                        }
                        let px = display.front();
                        if let Err(e) = remote::write_ppm(path, &px, display.w as usize, display.h as usize) {
                            eprintln!("launcher: remote: {path}: {e}");
                        }
                    }
                    _ => eprintln!("launcher: remote: unknown command {line:?}"),
                }
            }
        }
        input.read(&mut events);
        let now = Instant::now();
        input.tick(now, &mut events);
        dirty |= app.tick(now);
        for e in events.drain(..) {
            dirty = true;
            match e {
                Event::Headphones(on) => app.headphones = on,
                Event::Press(k) => match app.key(k) {
                    Action::None => {}
                    Action::Launch(g) => {
                        launch(&mut app, &mut display, &mut ui, &mut input, &bin_dir, g, &[]);
                        break;
                    }
                    Action::PowerOff => {
                        app.powering_off = true;
                        display.draw(&mut ui, |c| view::draw(c, &mut app));
                        display.settle();
                        power_off();
                    }
                    Action::ScreenOff => {
                        // (Keys queued behind this one are dropped with it.)
                        screen_off(&app, &mut input);
                        break;
                    }
                    Action::Exit => {
                        display.release();
                        return Ok(0);
                    }
                },
            }
        }
    }
}

/// Screen off (panel asleep, backlight off) until any button; that press
/// only wakes the screen.
fn screen_off(app: &App, input: &mut Input) {
    let _ = gb350_kms::control::set_panel(false);
    app.settings.backlight_off();
    let mut events = vec![];
    'wait: loop {
        let mut fds: Vec<libc::pollfd> = input.fds().iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        input.read(&mut events);
        for e in events.drain(..) {
            if matches!(e, Event::Press(_)) {
                break 'wait;
            }
        }
    }
    let _ = gb350_kms::control::set_panel(true);
    app.settings.apply_brightness();
    input.resync();
}

/// Runs a game in the emulator, handing it the display, buttons and sound.
fn launch(app: &mut App, display: &mut Display, ui: &mut gb350_ui::Ui, input: &mut Input, bin_dir: &Path, g: u32, extra: &[(String, String)]) {
    let game = &app.lib.games[g as usize];
    let sys = &SYSTEMS[game.sys];
    let Some(core) = sys.core else {
        app.toast(format!("There is no {} emulator yet", sys.name));
        return;
    };
    let bin = bin_dir.join(core);
    if !bin.exists() {
        app.toast(format!("{} is missing", bin.display()));
        return;
    }
    let name = game.name.clone();
    let path = app.lib.full_path(g);
    app.loading = Some(name.clone());
    display.settle();
    display.draw(ui, |c| view::draw(c, app));
    display.release();

    let log_path = Library::data_dir(&app.lib.root).join("retro.log");
    let log = std::fs::File::create(&log_path).ok();
    let out = log.as_ref().and_then(|f| f.try_clone().ok()).map(Stdio::from).unwrap_or_else(Stdio::null);
    let err = log.map(Stdio::from).unwrap_or_else(Stdio::null);
    eprintln!("launcher: starting {} ({})", name, path.display());
    let t = Instant::now();
    let status = Command::new(&bin)
        .arg(&path)
        .envs(app.settings.retro_env())
        .env("RETRO_TITLE", &name)
        .env("RETRO_SYSTEM", sys.name)
        .envs(extra.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(out)
        .stderr(err)
        .status();
    eprintln!("launcher: {name} ended after {} s: {status:?}", t.elapsed().as_secs());

    if let Err(e) = display.take() {
        eprintln!("launcher: take display: {e}");
    }
    input.resync();
    app.headphones = input.headphones;
    app.loading = None;
    app.after_game(g);
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => app.toast(format!("The game stopped with an error ({s}); see gb350/launcher/retro.log")),
        Err(e) => app.toast(format!("Could not start {}: {e}", bin.display())),
    }
}

/// Asks PID 1 (the agent) to shut down cleanly: it saves logs, releases the
/// SD card and turns the power off.
fn power_off() -> ! {
    unsafe {
        libc::sync();
        libc::kill(1, libc::SIGUSR2);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// Snapshot mode (development, runs on any host): drives the screens with a
/// script against a card's library and saves frames as PNGs, e.g.
/// `launcher --snapshot /Volumes/card /tmp/shots 'shot:lib,r,down,shot:sfc'`.
fn snapshot(root: &Path, out: &Path, script: &str) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut ui = gb350_ui::Ui::new();
    let settings = settings::Settings::load(out.join("settings.conf"));
    let t0 = Instant::now();
    let lib = Library::scan(root);
    eprintln!("snapshot: {} games in {} systems ({} ms)", lib.total(), lib.systems_used(), t0.elapsed().as_millis());
    let previews = preview::Previews::new(view::PREVIEW_W, view::PREVIEW_H);
    let mut app = App::new(lib, settings, previews);
    app.battery = Some((76, "Discharging".into()));
    let (w, h) = (640usize, 480usize);
    let mut px = vec![0u32; w * h];
    for step in script.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(name) = step.strip_prefix("shot:") {
            // Let screenshots load first.
            for _ in 0..100 {
                let t = Instant::now();
                view::draw(&mut gb350_ui::Canvas::new(&mut px, w as i32, h as i32, w, &mut ui), &mut app);
                if !app.previews.pending() {
                    eprintln!("snapshot: {name}: drawn in {} ms (host)", t.elapsed().as_millis());
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                app.previews.collect();
            }
            let path = out.join(format!("{name}.png"));
            let f = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
            enc.write_header().and_then(|mut wr| wr.write_image_data(&rgb)).map_err(|e| e.to_string())?;
        } else if let Some(text) = step.strip_prefix("type:") {
            app.set_query(text);
        } else if let Some(k) = input::Key::parse(step) {
            if let Action::Launch(g) = app.key(k) {
                app.loading = Some(app.lib.games[g as usize].name.clone());
            }
        } else if step == "done" {
            app.loading = None;
        } else {
            return Err(format!("unknown step {step}"));
        }
    }
    Ok(())
}
