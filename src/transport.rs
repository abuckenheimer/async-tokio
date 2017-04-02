use std::io;
use std::net::SocketAddr;
use cpython::*;
use futures::{unsync, Async, AsyncSink, Stream, Future, Poll, Sink};
use bytes::{Bytes, BytesMut};
use tokio_io::{AsyncRead};
use tokio_io::codec::{Encoder, Decoder, Framed};
use tokio_core::net::TcpStream;

use utils;
use unsafepy::{GIL, Handle, Sender};

// Transport factory
pub type TransportFactory = fn(Handle, &PyObject, TcpStream, SocketAddr) -> io::Result<()>;


pub enum TokioTcpTransportMessage {
    Bytes(PyBytes),
    Close,
}


py_class!(pub class TokioTcpTransport |py| {
    data handle: Handle;
    data transport: Sender<TokioTcpTransportMessage>;

    def get_extra_info(&self, _name: PyString,
                       default: Option<PyObject> = None ) -> PyResult<PyObject> {
        Ok(
            if let Some(ob) = default {
                ob
            } else {
                py.None()
            }
        )
    }

    //
    // write bytes to transport
    //
    def write(&self, data: PyBytes) -> PyResult<PyObject> {
        //let bytes = Bytes::from(data.data(py));
        let _ = self.transport(py).send(TokioTcpTransportMessage::Bytes(data));
        Ok(py.None())
    }

    //def drain(&self) -> PyResult<TokioFuture> {
    //    let fut = create_future(py, self.handle(py).clone())?;
    //    fut.set_result(py, py.None())?;
    //    Ok(fut)
    //}

    //
    // close transport
    //
    def close(&self) -> PyResult<PyObject> {
        let _ = self.transport(py).send(TokioTcpTransportMessage::Close);
        Ok(py.None())
    }

});


pub fn tcp_transport_factory(handle: Handle, factory: &PyObject,
                             socket: TcpStream, _peer: SocketAddr) -> Result<(), io::Error> {
    let gil = Python::acquire_gil();
    let py = gil.python();

    // create protocol
    let proto = match factory.call(py, NoArgs, None) {
        Ok(proto) => proto,
        Err(_) => {
            // TODO: log exception to loop logging facility
            return Err(io::Error::new(
                io::ErrorKind::Other, "Protocol factory failure"));
        }
    };

    // create transport and then call connection_made on protocol
    let transport = match TcpTransport::new(py, handle.clone(), socket, proto) {
        Err(err) =>
            return Err(io::Error::new(
                io::ErrorKind::Other, format!("Python protocol error: {:?}", err))),
        Ok(transport) => {
            debug!("connectino_made");
            let res = transport.connection_made.call(
                py, PyTuple::new(py, &[transport.transport.clone_ref(py).into_object()]), None);

            if let Err(err) = res {
                return Err(io::Error::new(
                    io::ErrorKind::Other, format!("Protocol.connection_made error: {:?}", err)))
            }

            transport
        }
    };

    handle.spawn(transport.map_err(|e| {
        println!("Error: {:?}", e);
    }));

    Ok(())
}


struct TcpTransport {
    framed: Framed<TcpStream, TcpTransportCodec>,
    intake: unsync::mpsc::UnboundedReceiver<TokioTcpTransportMessage>,

    //protocol: PyObject,
    transport: TokioTcpTransport,
    connection_made: PyObject,
    connection_lost: PyObject,
    data_received: PyObject,
    //eof_received: PyObject,

    buf: Option<PyBytes>,
    flushed: bool
}

impl TcpTransport {

    fn new(py: Python, handle: Handle, socket: TcpStream, protocol: PyObject) -> PyResult<TcpTransport> {
        let (tx, rx) = unsync::mpsc::unbounded();

        let transport = TokioTcpTransport::create_instance(py, handle, Sender::new(tx))?;
        let connection_made = protocol.getattr(py, "connection_made")?;
        let connection_lost = protocol.getattr(py, "connection_lost")?;
        let data_received = protocol.getattr(py, "data_received")?;
        //let eof_received = protocol.getattr(py, "eof_received")?;

        Ok(TcpTransport {
            framed: socket.framed(TcpTransportCodec),
            intake: rx,

            //protocol: protocol,
            transport: transport,
            connection_made: connection_made,
            connection_lost: connection_lost,
            data_received: data_received,
            //eof_received: eof_received,

            buf: None,
            flushed: false,
        })
    }

    fn read_from_socket(&mut self) -> Poll<(), io::Error> {
        // poll for incoming data
        match self.framed.poll() {
            Ok(Async::Ready(Some(bytes))) => {
                // trace!("Data recv: {}", bytes.len());
                let gil = Python::acquire_gil();
                let py = gil.python();

                let b = PyBytes::new(py, &bytes).into_object();
                let res = self.data_received.call(py, PyTuple::new(py, &[b]), None);

                match res {
                    Err(err) => {
                        debug!("data_received error {:?}", &err);
                        err.print(py);
                    }
                    _ => (),
                }

                self.read_from_socket()
            },
            Ok(Async::Ready(None)) => {
                // Protocol.connection_lost(None)
                debug!("connectino_lost");
                let gil = Python::acquire_gil();
                let py = gil.python();
                let res = self.connection_lost.call(py, PyTuple::new(py, &[py.None()]), None);

                match res {
                    Err(err) => {
                        debug!("connection_lost error {:?}", &err);
                        err.print(py);
                    }
                    _ => (),
                }

                Ok(Async::Ready(()))
            },
            Ok(Async::NotReady) => {
                Ok(Async::NotReady)
            },
            Err(err) => {
                debug!("connection_lost: {:?}", err);
                // Protocol.connection_lost(exc)
                let gil = Python::acquire_gil();
                let py = gil.python();

                let mut e = utils::os_error(py, &err);
                let res = self.connection_lost.call(py, PyTuple::new(py, &[e.instance(py)]), None);

                match res {
                    Err(err) => {
                        debug!("connection_lost error {:?}", &err);
                        err.print(py);
                    }
                    _ => (),
                }

                Err(err)
            }
        }
    }
}


impl Future for TcpTransport
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Async::Ready(_) = self.read_from_socket()? {
            return Ok(Async::Ready(()))
        }

        loop {
            let bytes = if let Some(bytes) = self.buf.take() {
                Some(bytes)
            } else {
                match self.intake.poll() {
                    Ok(Async::Ready(Some(msg))) => {
                        match msg {
                            TokioTcpTransportMessage::Bytes(bytes) =>
                                Some(bytes),
                            TokioTcpTransportMessage::Close =>
                                return Ok(Async::Ready(())),
                        }
                    }
                    Ok(_) => None,
                    Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
                }
            };

            if let Some(bytes) = bytes {
                self.flushed = false;

                match self.framed.start_send(bytes) {
                    Ok(AsyncSink::NotReady(bytes)) => {
                        self.buf = Some(bytes);
                        break
                    }
                    Ok(AsyncSink::Ready) => continue,
                    Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
                }
            } else {
                break
            }
        }

        // flush sink
        if !self.flushed {
            self.flushed = self.framed.poll_complete()?.is_ready();
        }

        Ok(Async::NotReady)
    }
}


struct TcpTransportCodec;

impl Decoder for TcpTransportCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if !src.is_empty() {
            Ok(Some(src.take().freeze()))
        } else {
            Ok(None)
        }
    }

}

impl Encoder for TcpTransportCodec {
    type Item = PyBytes;
    type Error = io::Error;

    fn encode(&mut self, msg: PyBytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.extend(msg.data(GIL::python()));
        Ok(())
    }

}