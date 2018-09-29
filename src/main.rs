extern crate crossbeam_channel as channel;
extern crate futures;
extern crate grep;
extern crate hyper;
extern crate ignore;
extern crate regex;
extern crate serde;
extern crate serde_json as json;
#[macro_use]
extern crate serde_derive;

#[macro_use]
pub mod errors;
pub mod ext;
pub mod params;
pub mod result;
pub mod search;

use std::env;
use std::fs;
use std::panic;
use std::path;
use std::process;
use std::thread;
use std::time;

use futures::{future, Stream};
use hyper::{Body, Client, Method, Request, Response, Server, StatusCode};
use hyper::header::CONTENT_TYPE;
use hyper::rt::Future;
use hyper::service::service_fn;

type BoxFuture = Box<Future<Item=Response<Body>, Error=hyper::Error> + Send>;

/// File name for storing connection parameters.
const PARAMS: &str = "PARAMS";
const LOCK: &str = "LOCK";
const LOCK_WAIT_MILLIS: u64 = 100;

/// Function to search and return JSON result.
fn find(params: params::QueryParams) -> Result<String, errors::Error> {
  let res = search::find(params.dir(), params.pattern(), Vec::new())?;
  json::to_string(&res).map_err(|err| errors::Error::new(err.to_string()))
}

fn service(req: Request<Body>) -> BoxFuture {
  match (req.method(), req.uri().path()) {
    (&Method::GET, "/ping") => {
      let mut response = Response::new(Body::empty());
      *response.status_mut() = StatusCode::OK;
      Box::new(future::ok(response))
    },
    (&Method::POST, "/search") => {
      let response = req
        .into_body()
        .concat2()
        .map(move |chunk| {
          let body = chunk.iter().cloned().collect::<Vec<u8>>();
          match json::from_slice::<params::QueryParams>(&body) {
            Ok(params) => {
              match find(params) {
                Ok(payload) => {
                  let mut response = Response::new(Body::from(payload));
                  *response.status_mut() = StatusCode::OK;
                  response.headers_mut().insert(
                    CONTENT_TYPE,
                    "application/json".parse().expect("correct content type value")
                  );
                  response
                }
                Err(error) => {
                  let mut response = Response::new(Body::from(error.to_string()));
                  *response.status_mut() = StatusCode::BAD_REQUEST;
                  response
                }
              }
            },
            Err(error) => {
              let mut response = Response::new(Body::from(error.to_string()));
              *response.status_mut() = StatusCode::BAD_REQUEST;
              response
            }
          }
        });
      Box::new(response)
    },
    _ => {
      let mut response = Response::new(Body::empty());
      *response.status_mut() = StatusCode::NOT_FOUND;
      Box::new(future::ok(response))
    }
  }
}

/// Creates and synchronises lock.
fn sync_lock(lock: &path::Path) {
  let interval = time::Duration::from_millis(LOCK_WAIT_MILLIS);
  let max_interval = time::Duration::from_millis(100 * LOCK_WAIT_MILLIS);
  let now = time::Instant::now();
  while let Err(cause) = fs::OpenOptions::new().write(true).create_new(true).open(lock) {
    thread::sleep(interval);
    assert!(now.elapsed() <= max_interval, "Failed to acquire the lock: {}", cause);
  }
}

/// Drops existing lock.
fn drop_lock(lock: &path::Path) {
  if let Err(cause) = fs::remove_file(lock) {
    eprintln!("Failed to drop the lock: {}", cause);
  }
}

fn with_lock<F, T>(dir: &path::Path, func: F) -> T
    where F: Fn() -> T + panic::UnwindSafe {
  let lock = dir.join(LOCK);
  sync_lock(lock.as_path());
  let res = panic::catch_unwind(func);
  drop_lock(lock.as_path());
  match res {
    Ok(answer) => answer,
    Err(cause) => panic!("Internal error: {:?}", cause)
  }
}

/// Load connection parameters, if available.
fn load_connection_params() -> Option<params::ConnectionParams> {
  let dir = env::current_dir().expect("Failed to retrieve current dir");
  let dir = dir.as_path();
  let res: Result<params::ConnectionParams, errors::Error> = with_lock(dir, || {
    let bytes = fs::read(dir.join(PARAMS))?;
    let opts = json::from_slice(&bytes)
      .map_err(|err| errors::Error::new(err.to_string()))?;
    Ok(opts)
  });
  res.ok()
}

/// Save connection parameters for this process.
fn save_connection_params(opts: &params::ConnectionParams) -> Option<()> {
  let dir = env::current_dir().expect("Failed to retrieve current dir");
  let dir = dir.as_path();
  let res: Result<(), errors::Error> = with_lock(dir, || {
    let bytes = json::to_vec(opts).map_err(|err| errors::Error::new(err.to_string()))?;
    fs::write(dir.join(PARAMS), &bytes)?;
    Ok(())
  });
  res.ok()
}

/// Ping the server, returns true if ping was successful.
fn ping(params: &params::ConnectionParams) -> bool {
  // TODO: Fix ping function.
  let client = Client::new();
  if let Ok(uri) = format!("http://{}/ping", params.address()).parse() {
    client.get(uri).wait().map(|r| r.status().is_success()).unwrap_or(false)
  } else {
    false
  }
}

fn main() {
  match load_connection_params().as_ref() {
    Some(ref params) if ping(params) => {
      println!("{}", params.address());
    },
    _ => {
      let initial_addr = ([127, 0, 0, 1], 0).into();
      let server = Server::bind(&initial_addr)
        .serve(|| service_fn(service));
      let opts = params::ConnectionParams::new(server.local_addr(), process::id());
      save_connection_params(&opts);
      println!("{}", opts.address());
      hyper::rt::run(server.map_err(|e| eprintln!("Server error: {}", e)));
    }
  }
}
