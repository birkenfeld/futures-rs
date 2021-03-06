#[macro_use]
extern crate futures_io;
extern crate futures_mio;
extern crate futures_tls;
extern crate net2;
#[macro_use]
extern crate futures;
extern crate httparse;
extern crate time;
#[macro_use]
extern crate log;

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use futures::Future;
use futures::stream::Stream;
use futures_io::{TaskIo, Ready, IoFuture};
use futures_mio::{Loop, LoopHandle, TcpStream, TcpListener};
use futures_tls::ServerContext;

mod request;
pub use self::request::{Request, RequestHeaders};

mod response;
pub use self::response::Response;

mod io2;
pub use io2::{Parse, Serialize};
use io2::{ParseStream, StreamWriter};

mod date;

pub trait Service<Req, Resp>: Send + Sync + 'static
    where Req: Send + 'static,
          Resp: Send + 'static
{
    type Fut: Future<Item = Resp>;

    fn process(&self, req: Req) -> Self::Fut;
}

impl<Req, Resp, Fut, F> Service<Req, Resp> for F
    where F: Fn(Req) -> Fut + Send + Sync + 'static,
          Fut: Future<Item = Resp>,
          Req: Send + 'static,
          Resp: Send + 'static
{
    type Fut = Fut;

    fn process(&self, req: Req) -> Fut {
        (self)(req)
    }
}

pub struct Server {
    addr: SocketAddr,
    workers: u32,
    tls: Option<Box<Fn() -> io::Result<ServerContext> + Send + Sync>>,
}

struct ServerData<S> {
    service: S,
    tls: Option<Box<Fn() -> io::Result<ServerContext> + Send + Sync>>,
}

impl Server {
    pub fn new(addr: &SocketAddr) -> Server {
        Server {
            addr: *addr,
            workers: 1,
            tls: None,
        }
    }

    pub fn workers(&mut self, workers: u32) -> &mut Server {
        if cfg!(unix) {
            self.workers = workers;
        }
        self
    }

    pub fn tls<F>(&mut self, tls: F) -> &mut Server
        where F: Fn() -> io::Result<ServerContext> + Send + Sync + 'static,
    {
        self.tls = Some(Box::new(tls));
        self
    }

    pub fn serve<Req, Resp, S>(&mut self, s: S) -> io::Result<()>
        where Req: Parse,
              Resp: Serialize,
              S: Service<Req, Resp>,
              <S::Fut as Future>::Error: From<Req::Error> + From<io::Error>, // TODO: simplify this?
    {
        let data = Arc::new(ServerData {
            service: s,
            tls: self.tls.take(),
        });

        let threads = (0..self.workers - 1).map(|i| {
            let mut lp = Loop::new().unwrap();
            let data = data.clone();
            let listener = self.listener(lp.handle());
            thread::Builder::new().name(format!("worker{}", i)).spawn(move || {
                lp.run(listener.and_then(move |l| {
                    l.incoming().for_each(move |(stream, _)| {
                        handle(stream, data.clone());
                        Ok(()) // TODO: error handling
                    })
                }))
            }).unwrap()
        }).collect::<Vec<_>>();

        let mut lp = Loop::new().unwrap();
        let listener = self.listener(lp.handle());
        lp.run(listener.and_then(move |l| {
            l.incoming().for_each(move |(stream, _)| {
                handle(stream, data.clone());
                Ok(()) // TODO: error handling
            })
        })).unwrap();

        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        Ok(())
    }

    fn listener(&self, handle: LoopHandle) -> Box<IoFuture<TcpListener>> {
        let listener = (|| {
            let listener = try!(net2::TcpBuilder::new_v4());
            try!(self.configure_tcp(&listener));
            try!(listener.reuse_address(true));
            try!(listener.bind(&self.addr));
            listener.listen(1024)
        })();

        match listener {
            Ok(l) => TcpListener::from_listener(l, &self.addr, handle),
            Err(e) => futures::failed(e).boxed()
        }
    }

    #[cfg(unix)]
    fn configure_tcp(&self, tcp: &net2::TcpBuilder) -> io::Result<()> {
        use net2::unix::*;

        if self.workers > 1 {
            try!(tcp.reuse_port(true));
        }

        Ok(())
    }

    #[cfg(windows)]
    fn configure_tcp(&self, _tcp: &net2::TcpBuilder) -> io::Result<()> {
        Ok(())
    }
}

trait IoStream: Read + Write + Stream<Item=Ready, Error=io::Error> {}

impl<T: ?Sized> IoStream for T
    where T: Read + Write + Stream<Item=Ready, Error=io::Error>
{}

fn handle<Req, Resp, S>(stream: TcpStream, data: Arc<ServerData<S>>)
    where Req: Parse,
          Resp: Serialize,
          S: Service<Req, Resp>,
          <S::Fut as Future>::Error: From<Req::Error> + From<io::Error>,
{
    let io = match data.tls {
        Some(ref tls) => {
            tls().unwrap().handshake(stream).map(|b| {
                Box::new(b) as
                    Box<IoStream<Item=Ready, Error=io::Error>>
            }).boxed()
        }
        None => {
            let stream = Box::new(stream) as
                    Box<IoStream<Item=Ready, Error=io::Error>>;
            futures::finished(stream).boxed()
        }
    };
    let io = io.and_then(|io| TaskIo::new(io)).map_err(From::from).and_then(|io| {
        let (reader, writer) = io.split();

        let input = ParseStream::new(reader).map_err(From::from);
        let responses = input.and_then(move |req| data.service.process(req));
        StreamWriter::new(writer, responses)
    });

    // Crucially use `.forget()` here instead of returning the future, allows
    // processing multiple separate connections concurrently.
    io.forget();
}
