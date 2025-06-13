use std::{
    cell::LazyCell,
    io::stdin,
    sync::{Mutex, mpsc::Receiver},
    thread,
};

pub(crate) static STDIN: Mutex<LazyCell<Receiver<String>>> = Mutex::new(LazyCell::new(|| {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in stdin().lines() {
            // safe as long as stdin doesn't close
            // the receiver is static so it will never drop
            sender.send(line.expect("stdin is closed")).unwrap();
        }
    });
    receiver
}));

#[macro_export]
macro_rules! send {
    () => {
        ::log::trace!("< ");
        println!();
    };

    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        ::log::trace!("< {msg}");
        println!("{msg}");
    }};
}

#[macro_export]
macro_rules! recv {
    () => {{
        let msg = $crate::io::STDIN
            .lock()
            .expect("stdin is closed")
            .recv()
            .unwrap();
        ::log::trace!("> {msg}");
        msg
    }};
}

#[macro_export]
macro_rules! try_recv {
    () => {{
        use std::sync::mpsc::TryRecvError;

        match $crate::io::STDIN
            .lock()
            .expect("stdin is closed")
            .try_recv()
        {
            Ok(msg) => {
                ::log::trace!("> {msg}");
                Some(msg)
            }
            Err(TryRecvError::Empty) => None,
            // the sender will only drop if the thread panics
            // so we should never see an Err(Disconnected)
            Err(TryRecvError::Disconnected) => unreachable!(),
        }
    }};
}
