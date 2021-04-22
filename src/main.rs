use std::collections::HashSet;
use std::convert::TryInto;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use rand::seq::{IteratorRandom, SliceRandom};
use rand::{rngs::StdRng, Rng, SeedableRng};

use nats::jetstream::{RetentionPolicy, StreamConfig};

const STREAM: &str = "exercise_stream";

fn idgen() -> u64 {
    static IDGEN: AtomicU64 = AtomicU64::new(0);
    IDGEN.fetch_add(1, SeqCst)
}

struct Cluster {
    clients: Vec<Consumer>,
    servers: Vec<Server>,
    paused: HashSet<usize>,
    rng: StdRng,
    unvalidated_consumers: HashSet<usize>,
    durability_model: DurabilityModel,
}

impl Cluster {
    fn start(args: &Args) -> Cluster {
        let seed = args.seed.unwrap_or(rand::thread_rng().gen());

        println!("Starting cluster exerciser with seed {}", seed);

        let rng = SeedableRng::seed_from_u64(seed);

        let servers: Vec<Server> =
            (0..args.servers).map(|i| server(&args.path, i as u16)).collect();

        // let servers come up
        std::thread::sleep(std::time::Duration::from_millis(1000));

        println!("creating testing stream {}", STREAM);

        {
            let nc = servers[0].nc();

            let _ = nc.delete_stream(STREAM);

            nc.create_stream(StreamConfig {
                name: STREAM.to_string(),
                retention: RetentionPolicy::WorkQueue,
                ..Default::default()
            })
            .expect("couldn't create exercise_stream");
        }

        let clients: Vec<Consumer> = servers
            .iter()
            .cycle()
            .enumerate()
            .take(args.clients as usize)
            .map(|(id, s)| {
                let consumer_name = format!("consumer_{}", id);
                println!("creating testing consumer {}", consumer_name);

                let nc = s.nc();
                Consumer {
                    inner: nc
                        .create_consumer(STREAM, &*consumer_name)
                        .expect("couldn't create consumer"),
                    observed: vec![],
                    id,
                }
            })
            .collect();

        Cluster {
            servers,
            clients,
            rng: rng,
            paused: Default::default(),
            durability_model: Default::default(),
            unvalidated_consumers: Default::default(),
        }
    }

    fn step(&mut self) {
        match self.rng.gen_range(0..50) {
            0 => self.restart_server(),
            1..=4 => self.pause_server(),
            5..=9 => self.resume_server(),
            10..=29 => self.publish(),
            30..=49 => self.consume(),
            _ => unreachable!("impossible choice"),
        }
        self.validate();
    }

    fn restart_server(&mut self) {
        let idx = self.rng.gen_range(0..self.servers.len());
        println!("restarting server {}", idx);

        self.servers[idx].restart();
        self.paused.remove(&idx);
    }

    fn pause_server(&mut self) {
        if self.paused.len() == self.servers.len() {
            // all servers already paused
            return;
        }
        let mut idx = self.rng.gen_range(0..self.servers.len());

        while self.paused.contains(&idx) {
            idx = self.rng.gen_range(0..self.servers.len());
        }

        println!("pausing server {}", idx);

        let pid = self.servers[idx].child.as_ref().unwrap().id();

        unsafe {
            if libc::kill(pid as libc::pid_t, libc::SIGSTOP) != 0 {
                panic!("{:?}", io::Error::last_os_error());
            }
        }

        self.paused.insert(idx);
    }

    fn resume_server(&mut self) {
        if self.paused.is_empty() {
            // nothing ot resume
            return;
        }

        let idx = *self.paused.iter().choose(&mut self.rng).unwrap();

        println!("resuming server {}", idx);

        let pid = self.servers[idx].child.as_ref().unwrap().id();

        unsafe {
            if libc::kill(pid as libc::pid_t, libc::SIGCONT) != 0 {
                panic!("{:?}", io::Error::last_os_error());
            }
        }

        self.paused.remove(&idx);
    }

    fn publish(&mut self) {
        let c = self.clients.choose(&mut self.rng).unwrap();
        println!("publishing message by client {}", c.id);
        let data = idgen().to_le_bytes();
        c.inner.nc.publish(STREAM, data).unwrap();
    }

    fn consume(&mut self) {
        let c = self.clients.choose_mut(&mut self.rng).unwrap();
        println!("consuming message by client {}", c.id);

        let proc_ret: io::Result<u64> = c.inner.process_timeout(|msg| {
            let id = u64::from_le_bytes((&*msg.data).try_into().unwrap());
            Ok(id)
        });

        if let Ok(id) = proc_ret {
            c.observed.push(id);
            self.unvalidated_consumers.insert(c.id);
        }
    }

    fn validate(&mut self) {
        // assert all consumers have witnessed messages in the correct order
        let unvalidated_consumers =
            std::mem::take(&mut self.unvalidated_consumers);

        for id in unvalidated_consumers {
            let c = &mut self.clients[id];
            let client_len = c.observed.len();
            let cluster_len = self.durability_model.observed.len();
            let shared_len = cluster_len.min(client_len);
            assert_eq!(
                self.durability_model.observed[..shared_len],
                c.observed[..shared_len],
                "observed messages must occur in the same order for all consumers",
            );

            if client_len > cluster_len {
                self.durability_model
                    .observed
                    .extend_from_slice(&c.observed[shared_len..]);
            }
        }
    }
}

struct Server {
    child: Option<Child>,
    port: u16,
    storage_dir: String,
    path: PathBuf,
    idx: u16,
}

impl Server {
    fn nc(&self) -> nats::Connection {
        nats::connect(&format!("localhost:{}", self.port)).unwrap()
    }

    fn restart(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        *self = server(&self.path, self.idx);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let _ = std::fs::remove_dir_all(&self.storage_dir);
    }
}

/// Starts a local NATS server that gets killed on drop.
fn server<P: AsRef<Path>>(path: P, idx: u16) -> Server {
    let port = idx + 44000;
    let storage_dir = format!("jetstream_test_{}", idx);
    let _ = std::fs::remove_dir_all(&storage_dir);

    let supercluster_conf = format!("confs/supercluster_{}.conf", idx);

    let child = Command::new(path.as_ref())
        .args(&["--port", &port.to_string()])
        .arg("-js")
        .args(&["-sd", &storage_dir])
        .args(&["-c", &supercluster_conf])
        .arg("-V")
        .arg("-D")
        .spawn()
        .expect("unable to spawn nats-server");

    Server {
        child: Some(child),
        port,
        storage_dir,
        path: path.as_ref().into(),
        idx,
    }
}

struct Consumer {
    inner: nats::jetstream::Consumer,
    observed: Vec<u64>,
    id: usize,
}

// every message
#[derive(Default, Debug)]
struct DurabilityModel {
    observed: Vec<u64>,
}

const USAGE: &str = "
Usage: exercise [--path=</path/to/nats-server>] [--seed=<#>] [--clients=<#>] [--servers=<#>] [--steps=<#>]

Options:
    --path=<p>      Path to nats-server binary [default: nats-server].
    --seed=<#>      Seed for driving faults [default: None].
    --clients=<#>   Number of concurrent clients [default: 2].
    --servers=<#>   Number of cluster servers [default: 3].
    --steps=<#>     Number of steps to take [default: 10000].
";

struct Args {
    path: PathBuf,
    seed: Option<u64>,
    clients: u8,
    servers: u8,
    steps: u64,
}

impl Default for Args {
    fn default() -> Args {
        Args {
            path: "nats-server".into(),
            seed: None,
            clients: 2,
            servers: 3,
            steps: 10000,
        }
    }
}

fn parse<'a, I, T>(mut iter: I) -> T
where
    I: Iterator<Item = &'a str>,
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::fmt::Debug,
{
    iter.next().expect(USAGE).parse().expect(USAGE)
}

impl Args {
    fn parse() -> Args {
        let mut args = Args::default();
        for raw_arg in std::env::args().skip(1) {
            let mut splits = raw_arg[2..].split('=');
            match splits.next().unwrap() {
                "path" => args.path = parse(&mut splits),
                "seed" => args.seed = Some(parse(&mut splits)),
                "clients" => args.clients = parse(&mut splits),
                "servers" => args.servers = parse(&mut splits),
                "steps" => args.steps = parse(&mut splits),
                other => panic!("unknown option: {}, {}", other, USAGE),
            }
        }
        args
    }
}

fn main() {
    let args = Args::parse();

    let mut cluster = Cluster::start(&args);

    for _ in 0..args.steps {
        cluster.step();
    }
}
