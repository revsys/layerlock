use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}
impl Response {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: String::new(),
        }
    }
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }
    pub fn body(mut self, body: &str) -> Self {
        self.body = body.into();
        self
    }
}
pub struct Server {
    pub host: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    pub fn new(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            let mut workers = vec![];
            while !signal.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        workers.push(thread::spawn(move || serve(stream, &*handler)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            host,
            stop,
            thread: Some(thread),
        }
    }
    pub fn reference(&self) -> String {
        format!("{}/team/base:sha256-test", self.host)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve(mut stream: TcpStream, handler: &dyn Fn(Request) -> Response) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let parts: Vec<_> = line.split_whitespace().collect();
    let method = parts[0].to_string();
    let path = parts[1].to_string();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().to_string());
    }
    let length = headers
        .get("content-length")
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let is_head = method == "HEAD";
    let response = handler(Request {
        method,
        path,
        headers,
        body: String::from_utf8(body).unwrap(),
    });
    let mut text = format!(
        "HTTP/1.1 {} Status\r\nConnection: close\r\nContent-Length: {}\r\n",
        response.status,
        response.body.len()
    );
    for (key, value) in response.headers {
        text.push_str(&format!("{key}: {value}\r\n"));
    }
    text.push_str("\r\n");
    if !is_head {
        text.push_str(&response.body);
    }
    // Timeout tests deliberately let the client disconnect first.
    let _ = stream.write_all(text.as_bytes());
}
