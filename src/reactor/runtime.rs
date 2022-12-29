use std::{future::Future, pin::Pin, sync::Mutex, thread};
use tokio::{
    runtime, sync::{mpsc, oneshot}, task
};

static GLOBAL_RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// 运行时
pub struct Runtime {
    msg_tx: mpsc::Sender<RuntimeMessage>,
    stop_rx: Option<oneshot::Receiver<()>>
}

/// 运行时消息
pub enum RuntimeMessage {
    Task(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    Stop
}

impl Runtime {
    /// 获取运行时消息发送通道
    pub fn global_sender() -> mpsc::Sender<RuntimeMessage> {
        let mut runtime = GLOBAL_RUNTIME.lock().unwrap();
        if runtime.is_none() {
            *runtime = Some(Runtime::new());
        }
        runtime.as_ref().unwrap().msg_tx.clone()
    }

    /// 销毁运行时
    pub fn drop_global() {
        let mut runtime = GLOBAL_RUNTIME.lock().unwrap();
        *runtime = None;
    }

    /// 创建运行时
    fn new() -> Runtime {
        assert!(runtime::Handle::try_current().is_err());
        //退出信号
        let (stop_tx, stop_rx) = oneshot::channel();
        //消息通道
        let (msg_tx, mut msg_rx) = mpsc::channel(256);

        //创建后台线程
        thread::Builder::new()
            .name("bkgnd-rt".to_owned())
            .spawn(move || {
                let runloop = async move {
                    while let Some(msg) = msg_rx.recv().await {
                        match msg {
                            RuntimeMessage::Task(task) => {
                                task::spawn_local(task);
                            },
                            RuntimeMessage::Stop => break
                        }
                    }
                };
                //单线程运行时
                let rt = runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let local = task::LocalSet::new();
                //运行
                rt.block_on(local.run_until(runloop));
                rt.block_on(local);
                //退出信号
                stop_tx.send(()).unwrap();
            })
            .expect("new background thread");

        Runtime {
            msg_tx,
            stop_rx: Some(stop_rx)
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.msg_tx.blocking_send(RuntimeMessage::Stop);
        //NOTE 不能直接WAIT线程对象，因为此时处于TLS销毁流程中，OS加了保护锁防止同时销毁
        self.stop_rx.take().unwrap().blocking_recv().unwrap();
        //FIXME
        //短暂挂起使线程调用栈完全退出
        thread::sleep(std::time::Duration::from_millis(100));
    }
}
