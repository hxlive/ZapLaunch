use std::{collections::VecDeque, io, path::PathBuf, sync::{mpsc, Arc, Condvar, Mutex, OnceLock}};
use zaplaunch_core::AppConfig;

enum Request {
    Save(AppConfig, PathBuf, Option<mpsc::Sender<io::Result<()>>>),
    Edit(AppConfig, AppConfig, PathBuf, Option<zaplaunch_sys::tray::ButtonHotkeyClient>, u64),
    Flush(mpsc::Sender<io::Result<()>>),
}

pub struct Writer(Arc<(Mutex<VecDeque<Request>>, Condvar)>);

impl Writer {
    pub(super) fn start(save: impl Fn(&AppConfig, &std::path::Path, bool) -> io::Result<()> + Send + 'static) -> io::Result<Self> {
        let queue = Arc::new((Mutex::new(VecDeque::<Request>::new()), Condvar::new()));
        let worker = queue.clone();
        std::thread::Builder::new().name("zaplaunch-save".into()).spawn(move || {
            let mut retry = None;
            let mut backup_warned = false;
            loop {
                let request = {
                    let (queue, wake) = &*worker;
                    let mut queue = queue.lock().unwrap();
                    while queue.is_empty() { queue = wake.wait(queue).unwrap(); }
                    queue.pop_front().unwrap()
                };
                match request {
                    Request::Edit(next, previous, dir, client, id) => {
                        let result = edit(&next, &previous, &dir, client, &save);
                        if result.is_ok() { retry = None; }
                        super::push_tray_event(zaplaunch_sys::tray::TrayEvent::ConfigSaved(id, result.map_err(|e| e.to_string())));
                        let warning = zaplaunch_core::take_backup_warning();
                        if warning && !backup_warned { warn("配置已保存，但备份更新失败。请检查配置目录权限和备份文件。".into()); }
                        backup_warned = warning;
                    }
                    Request::Save(config, dir, reply) => {
                        let backup = reply.is_some();
                        let result = save(&config, &dir, backup);
                        if result.is_ok() { retry = None; }
                        if let Some(reply) = reply { let _ = reply.send(result); }
                        else if let Err(e) = result {
                            if retry.is_none() { warn(format!("启动统计未保存：{e}。下次保存或退出时会重试，当前内存记录仍保留。")); }
                            retry = Some((config, dir));
                        }
                        let warning = zaplaunch_core::take_backup_warning();
                        if warning && !backup_warned { warn("配置已保存，但备份更新失败。请检查配置目录权限和备份文件。".into()); }
                        if backup { backup_warned = warning; }
                    }
                    Request::Flush(reply) => {
                        let result = if let Some((config, dir)) = &retry { save(config, dir, false) } else { Ok(()) };
                        if result.is_ok() { retry = None; }
                        let _ = reply.send(result);
                    }
                }
            }
        })?;
        Ok(Self(queue))
    }

    fn enqueue(&self, request: Request) {
        let (queue, wake) = &*self.0;
        let mut queue = queue.lock().unwrap();
        if let Request::Save(_, dir, None) = &request {
            if matches!(queue.back(), Some(Request::Save(_, previous, None)) if previous == dir) { queue.pop_back(); }
        }
        queue.push_back(request);
        wake.notify_one();
    }

    pub fn save(&self, config: &AppConfig, backup: bool) -> io::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.enqueue(Request::Save(config.clone(), zaplaunch_core::config_dir(), backup.then_some(tx)));
        if backup { rx.recv().map_err(|_| io::Error::other("保存线程已退出"))? } else { Ok(()) }
    }

    pub fn edit(&self, next: AppConfig, previous: AppConfig, client: Option<zaplaunch_sys::tray::ButtonHotkeyClient>, id: u64) {
        self.enqueue(Request::Edit(next, previous, zaplaunch_core::config_dir(), client, id));
    }

    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        self.enqueue(Request::Flush(tx));
        let result = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|e| {
            Err(io::Error::other(match e {
                mpsc::RecvTimeoutError::Timeout => "等待保存超时，最后的启动统计可能未保存",
                mpsc::RecvTimeoutError::Disconnected => "保存线程已退出",
            }))
        });
        if let Err(e) = result {
            zaplaunch_sys::show_error(&format!("退出前仍无法保存启动统计：{e}"));
        }
    }
}

fn edit(next: &AppConfig, previous: &AppConfig, dir: &std::path::Path, client: Option<zaplaunch_sys::tray::ButtonHotkeyClient>, save: impl Fn(&AppConfig, &std::path::Path, bool) -> io::Result<()>) -> io::Result<Vec<(usize, usize)>> {
    let keys_changed = super::button_hotkey_fields(next) != super::button_hotkey_fields(previous);
    let mut failed = Vec::new();
    if keys_changed {
        let (keys, invalid) = super::button_hotkeys(next);
        failed = invalid;
        failed.extend(client.and_then(|c| c.update(keys)).ok_or_else(|| io::Error::other("按钮快捷键更新失败，请重试"))?);
    }
    let auto_changed = next.auto_start != previous.auto_start;
    let exe = zaplaunch_sys::current_exe_path();
    let result = if auto_changed && !zaplaunch_sys::set_auto_start(next.auto_start, &exe) {
        Err(io::Error::other("开机自启更新失败"))
    } else { save(next, dir, true) };
    if let Err(e) = result {
        let mut message = e.to_string();
        if auto_changed && !zaplaunch_sys::set_auto_start(previous.auto_start, &exe) { message.push_str("；旧开机自启恢复失败"); }
        if keys_changed && client.and_then(|c| c.update(super::button_hotkeys(previous).0)).map(|f| !f.is_empty()).unwrap_or(true) {
            message.push_str("；旧按钮快捷键恢复失败，请重启");
        }
        return Err(io::Error::other(message));
    }
    Ok(failed)
}

fn warn(message: String) {
    eprintln!("{message}");
    #[cfg(not(test))]
    {
    let fallback = message.clone();
    if slint::invoke_from_event_loop(move || zaplaunch_sys::show_error(&message)).is_err() {
        zaplaunch_sys::show_error(&fallback);
    }
    }
}

static WRITER: OnceLock<io::Result<Writer>> = OnceLock::new();
pub fn get() -> io::Result<&'static Writer> {
    WRITER.get_or_init(|| Writer::start(zaplaunch_core::save_to_with_backup))
        .as_ref().map_err(|e| io::Error::other(e.to_string()))
}

pub fn flush() { if let Some(Ok(writer)) = WRITER.get() { writer.flush(); } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flush_retries_failed_usage() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        let writer = Writer::start(move |_, _, _| {
            if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { Err(io::Error::other("fixture")) } else { Ok(()) }
        }).unwrap();
        writer.enqueue(Request::Save(AppConfig::default(), PathBuf::new(), None));
        writer.flush();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        writer.flush();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
    #[test]
    fn slow_save_coalesces_usage_without_reordering_edits_and_returns_errors() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let writer = Writer::start(move |config, _, backup| {
            if config.history_limit == 1 { started_tx.send(()).unwrap(); release_rx.recv().unwrap(); }
            log.lock().unwrap().push((config.history_limit, backup));
            if config.history_limit == 4 { Err(io::Error::other("fixture")) } else { Ok(()) }
        }).unwrap();
        let request = |n, reply| { let mut c = AppConfig::default(); c.history_limit = n; Request::Save(c, PathBuf::new(), reply) };
        writer.enqueue(request(1, None)); started_rx.recv().unwrap();
        writer.enqueue(request(2, None)); writer.enqueue(request(3, None));
        let (tx, rx) = mpsc::channel(); writer.enqueue(request(4, Some(tx)));
        writer.enqueue(request(5, None)); release_tx.send(()).unwrap();
        assert!(rx.recv().unwrap().is_err()); writer.flush();
        assert_eq!(*seen.lock().unwrap(), [(1,false),(3,false),(4,true),(5,false)]);
    }
}
