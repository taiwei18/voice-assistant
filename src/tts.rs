//! 语音播报：Windows 自带的 SAPI，跑在独立线程上（COM 对象不跨线程）。可以中途打断。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use tokio::sync::oneshot;
use windows::Win32::Media::Speech::{ISpVoice, SPF_ASYNC, SPF_PURGEBEFORESPEAK, SpVoice};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::HSTRING;

type Job = (String, i32, oneshot::Sender<()>);

pub struct Tts {
    tx: mpsc::Sender<Job>,
    stop: Arc<AtomicBool>,
}

impl Tts {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("tts".into())
            .spawn(move || unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let voice: Option<ISpVoice> = CoCreateInstance(&SpVoice, None, CLSCTX_ALL).ok();
                for (text, rate, done) in rx {
                    if let Some(voice) = &voice {
                        stop2.store(false, Ordering::SeqCst);
                        let _ = voice.SetRate(rate);
                        // 异步朗读，每 50ms 看一次要不要打断
                        if voice.Speak(&HSTRING::from(text), SPF_ASYNC.0 as u32, None).is_ok() {
                            loop {
                                if stop2.load(Ordering::SeqCst) {
                                    let _ = voice.Speak(&HSTRING::new(), (SPF_ASYNC.0 | SPF_PURGEBEFORESPEAK.0) as u32, None);
                                    break;
                                }
                                // 朗读队列读完时这个事件变为有信号
                                if WaitForSingleObject(voice.SpeakCompleteEvent(), 50) == WAIT_OBJECT_0 {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = done.send(());
                }
            })
            .expect("启动播报线程失败");
        Self { tx, stop }
    }

    /// 朗读并等待读完（或被 stop 打断）
    pub async fn speak(&self, text: &str, rate: i32) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.tx.send((text.to_string(), rate, done_tx)).is_ok() {
            let _ = done_rx.await;
        }
    }

    /// 打断正在进行的朗读
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 会真的出声：念一段长句，0.5 秒后打断，应当很快停下。cargo test -- --ignored
    #[test]
    #[ignore]
    fn stop_interrupts_speech() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let tts = Tts::new();
        rt.block_on(async {
            let start = std::time::Instant::now();
            let speak = tts.speak("这是一段比较长的测试句子，用来检查播报能不能被中途打断，正常情况下要念好几秒钟才能念完。", 0);
            tokio::pin!(speak);
            tokio::select! {
                _ = &mut speak => panic!("还没打断就念完了"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => tts.stop(),
            }
            speak.await;
            println!("打断后停止，总用时 {:?}", start.elapsed());
            assert!(start.elapsed() < std::time::Duration::from_millis(1500));
        });
    }
}
