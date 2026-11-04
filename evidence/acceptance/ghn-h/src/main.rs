use janus_core::metering::*;
use std::sync::{atomic::{AtomicI64, Ordering}, Arc};
use std::time::Duration;

fn q(limit: u64, window: Window) -> Quota { Quota { limit, window } }
fn now() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 }
fn rep(id: &str, ok: bool, note: String) { println!("{} {} {}", id, if ok { "PASS" } else { "FAIL" }, note); }
fn key(identity: &str, meter: &str, period: u64, t: i64) -> MeterKey {
    MeterKey { identity: identity.into(), meter: meter.into(), period_secs: period, window_start: if period == 0 { 0 } else { t - t % period as i64 } }
}
fn clocked<S: UsageStore>(m: Meter<S>, c: Arc<AtomicI64>) -> Meter<S> { m.with_clock(move || c.load(Ordering::SeqCst)) }

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let path = &args[2];
        match args[1].as_str() {
            "sqlite-write" => {
                let s = SqliteUsageStore::open(path).unwrap();
                println!("durability={:?}", s.durability());
                let m = Meter::new(s, vec![("budget".into(), q(100, Window::Total))]).unwrap();
                let c = m.charge("w1", "id1", "budget", 7).await.unwrap();
                println!("charge={:?}", c);
            }
            "sqlite-read" => {
                let s = SqliteUsageStore::open(path).unwrap();
                let m = Meter::new(s, vec![("budget".into(), q(100, Window::Total))]).unwrap();
                println!("check={:?}", m.check("id1", "budget").await.unwrap());
                println!("count={}", m.store().get(&key("id1", "budget", 0, 0)).await.unwrap());
            }
            "sqlite-hold" => {
                let s = SqliteUsageStore::open(path).unwrap();
                println!("held");
                use std::io::Write; std::io::stdout().flush().unwrap();
                tokio::time::sleep(Duration::from_secs(20)).await;
                drop(s);
            }
            "sqlite-probe" => {
                match SqliteUsageStore::open(path) {
                    Ok(_) => println!("OPEN_OK"),
                    Err(e) => println!("OPEN_ERR {} / {:?}", e, e),
                }
            }
            "sqlite-crash" => {
                use std::io::Write;
                let s = SqliteUsageStore::open(path).unwrap();
                let m = Meter::new(s, vec![("budget".into(), q(1_000_000_000, Window::Total))]).unwrap();
                let mut i = 0u64;
                loop {
                    m.charge(&format!("c{i}"), "crash", "budget", 1).await.unwrap();
                    i += 1;
                    println!("acked {i}");
                    std::io::stdout().flush().unwrap();
                }
            }
            "sqlite-count" => {
                let s = SqliteUsageStore::open(path).unwrap();
                println!("count={}", s.get(&key("crash", "budget", 0, 0)).await.unwrap());
            }
            "sqlite-closed" => { let s = SqliteUsageStore::open(path).unwrap(); let m = Meter::new(s, vec![("budget".into(), q(100, Window::Total))]).unwrap(); m.charge("z","i","budget",1).await.unwrap(); drop(m); println!("closed"); }
            _ => panic!("bad"),
        }
        return;
    }

    // D65
    {
        let r1 = Meter::new(MemoryUsageStore::new(), vec![("requests".into(), q(10, Window::Total))]);
        let r2 = Meter::new_allowing_volatile_total(MemoryUsageStore::new(), vec![("requests".into(), q(10, Window::Total))]);
        let dir = std::env::temp_dir().join(format!("qa-h-d65-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r3 = Meter::new(SqliteUsageStore::open(dir.join("u.db")).unwrap(), vec![("requests".into(), q(10, Window::Total))]);
        let e = r1.as_ref().err().map(|e| format!("{e} / {e:?}"));
        rep("D65", r1.is_err() && matches!(r1.as_ref().err(), Some(UsageError::VolatileTotal{..})) && r2.is_ok() && r3.is_ok(),
            format!("new(memory,Total)={:?}; allowing_volatile_total ok={}; sqlite Total ok={}", e, r2.is_ok(), r3.is_ok()));
    }
    // D66
    {
        let mut bad = 0; let mut detail = String::new();
        for t in 0..300 {
            let m = Arc::new(Meter::new(MemoryUsageStore::new(), vec![("r".into(), q(10, Window::Fixed{period: Duration::from_secs(3600)}))]).unwrap());
            let c = m.charge("pre", "a", "r", 9).await.unwrap();
            assert!(c.is_allowed());
            let (m1, m2) = (m.clone(), m.clone());
            let h1 = tokio::spawn(async move { m1.charge("x1", "a", "r", 1).await.unwrap() });
            let h2 = tokio::spawn(async move { m2.charge("x2", "a", "r", 1).await.unwrap() });
            let (a, b) = (h1.await.unwrap(), h2.await.unwrap());
            let allowed = [a, b].iter().filter(|c| matches!(c, Charge::Allowed{..})).count();
            let exceeded = [a, b].iter().filter(|c| matches!(c, Charge::Exceeded{..})).count();
            let cnt = m.store().get(&key("a", "r", 3600, now())).await.unwrap();
            if allowed != 1 || exceeded != 1 || cnt != 10 { bad += 1; detail = format!("trial {t}: {a:?} {b:?} count={cnt}"); }
        }
        // also sqlite
        let dir = std::env::temp_dir().join(format!("qa-h-d66-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = Arc::new(Meter::new(SqliteUsageStore::open(dir.join("u.db")).unwrap(), vec![("r".into(), q(10, Window::Total))]).unwrap());
        m.charge("pre", "a", "r", 9).await.unwrap();
        let (m1, m2) = (m.clone(), m.clone());
        let h1 = tokio::spawn(async move { m1.charge("x1", "a", "r", 1).await.unwrap() });
        let h2 = tokio::spawn(async move { m2.charge("x2", "a", "r", 1).await.unwrap() });
        let (a, b) = (h1.await.unwrap(), h2.await.unwrap());
        let cnt = m.store().get(&key("a", "r", 0, 0)).await.unwrap();
        let sok = [a, b].iter().filter(|c| c.is_allowed()).count() == 1 && cnt == 10;
        // Allowed{used,limit} nearest exhaustion check
        let m3 = Meter::new(MemoryUsageStore::new(), vec![("r".into(), q(10, Window::Fixed{period: Duration::from_secs(60)})), ("r".into(), q(100, Window::Fixed{period: Duration::from_secs(86400)}))]).unwrap();
        let c3 = m3.charge("n1", "a", "r", 3).await.unwrap();
        rep("D66", bad == 0 && sok && c3 == (Charge::Allowed{used:3, limit:10}),
            format!("memory 300 trials of 2 concurrent charges at 9/10: bad={bad} {detail}; sqlite: {a:?} {b:?} count={cnt}; two-quota allowed={c3:?}"));
    }
    // D67
    {
        let m = Meter::new(MemoryUsageStore::new(), vec![("r".into(), q(10, Window::Fixed{period: Duration::from_secs(3600)})), ("r".into(), q(100, Window::Fixed{period: Duration::from_secs(86400)}))]).unwrap();
        let t = now();
        let c1 = m.charge("r1", "a", "r", 8).await.unwrap();
        let hourly = |t| key("a","r",3600,t); let daily = |t| key("a","r",86400,t);
        let (h0, d0) = (m.store().get(&hourly(t)).await.unwrap(), m.store().get(&daily(t)).await.unwrap());
        let c2 = m.charge("r2", "a", "r", 5).await.unwrap();
        let (h1, d1) = (m.store().get(&hourly(t)).await.unwrap(), m.store().get(&daily(t)).await.unwrap());
        let c1b = m.charge("r1", "a", "r", 8).await.unwrap();
        let (h2, d2) = (m.store().get(&hourly(t)).await.unwrap(), m.store().get(&daily(t)).await.unwrap());
        let c2b = m.charge("r2", "a", "r", 5).await.unwrap();
        let ok = matches!(c2, Charge::Exceeded{..}) && (h0,d0)==(h1,d1) && c1==c1b && (h2,d2)==(h0,d0) && h0==8 && matches!(c2b, Charge::Exceeded{..});
        rep("D67", ok, format!("first={c1:?} counts=({h0},{d0}); exceeded={c2:?} counts=({h1},{d1}); repeat r1={c1b:?} counts=({h2},{d2}); repeat exceeded r2={c2b:?}"));
    }
    // D68
    {
        let m = Meter::new(MemoryUsageStore::new(), vec![("cost".into(), q(100, Window::Fixed{period: Duration::from_secs(3600)}))]).unwrap();
        let t = now();
        let before = m.check("a", "cost").await.unwrap();
        m.record("c1", "a", "cost", 100).await.unwrap();
        let mut all = true; let mut outs = vec![];
        for _ in 0..5 { let c = m.check("a", "cost").await.unwrap(); outs.push(c); all &= matches!(c, Charge::Exceeded{..}); }
        let cnt = m.store().get(&key("a","cost",3600,t)).await.unwrap();
        rep("D68", all && cnt == 100 && before.is_allowed(), format!("before={before:?} after-record checks={:?} count={cnt}", outs[0]));
    }
    // D69
    {
        let m = Meter::new(MemoryUsageStore::new(), vec![("cost".into(), q(100, Window::Fixed{period: Duration::from_secs(3600)}))]).unwrap();
        let t = now();
        m.record("seed", "a", "cost", 90).await.unwrap();
        m.record("r1", "a", "cost", 50).await.unwrap();
        m.record("r1", "a", "cost", 50).await.unwrap();
        let cnt = m.store().get(&key("a","cost",3600,t)).await.unwrap();
        let c = m.check("a", "cost").await.unwrap();
        rep("D69", cnt == 140 && matches!(c, Charge::Exceeded{..}), format!("count={cnt} check={c:?}"));
    }
    // D70
    {
        let base = { let n = now() + 600; n - n % 60 };
        let clock = Arc::new(AtomicI64::new(base));
        let m = clocked(Meter::new(MemoryUsageStore::new(), vec![("r".into(), q(10, Window::Fixed{period: Duration::from_secs(60)}))]).unwrap(), clock.clone());
        m.charge("c1", "a", "r", 3).await.unwrap();
        clock.store(base+65, Ordering::SeqCst);
        m.charge("c2", "a", "r", 2).await.unwrap();
        let w1 = key("a","r",60,base); let w2 = key("a","r",60,base+65);
        let (w1a, w2a) = (m.store().get(&w1).await.unwrap(), m.store().get(&w2).await.unwrap());
        let r1 = m.refund("c1", "a", "r").await.unwrap();
        let (w1b, w2b) = (m.store().get(&w1).await.unwrap(), m.store().get(&w2).await.unwrap());
        let r2 = m.refund("c1", "a", "r").await.unwrap();
        rep("D70", w1a==3 && w2a==2 && w1b==0 && w2b==2 && r1 && !r2, format!("before W1={w1a} W2={w2a}; first refund={r1}; after W1={w1b} W2={w2b}; second refund={r2}"));
    }
    // D71
    {
        let t = now();
        let clock = Arc::new(AtomicI64::new(t + 12*3600)); // keep the 24h window's retain_until well beyond compact times
        let mk = || clocked(Meter::new(MemoryUsageStore::new(), vec![("r".into(), q(1000, Window::Fixed{period: Duration::from_secs(86400)}))]).unwrap(), clock.clone());
        let wk = key("a","r",86400,t + 12*3600);
        // a
        let m = mk(); m.charge("a1","a","r",1).await.unwrap(); m.settle("a1","a","r").await.unwrap();
        let a = m.refund("a1","a","r").await.unwrap();
        // b
        let m = mk(); m.charge("r1","a","r",4).await.unwrap();
        let cb = m.store().compact(t + 59*60).await.unwrap();
        let b = m.refund("r1","a","r").await.unwrap();
        // c
        let m = mk(); m.charge("r2","a","r",4).await.unwrap();
        let cc = m.store().compact(t + 61*60).await.unwrap();
        let c = m.refund("r2","a","r").await.unwrap();
        let cnt_c = m.store().get(&wk).await.unwrap();
        // d
        let m = mk(); m.record("r3","a","r",5).await.unwrap();
        let cd = m.store().compact(t + 61*60).await.unwrap();
        m.record("r3","a","r",5).await.unwrap();
        let cnt_d = m.store().get(&wk).await.unwrap();
        rep("D71-a", !a, format!("charge, settle, then refund -> {a} (expected false)"));
        rep("D71-b", b && cb == 0, format!("charge r1 at T, compact(T+59m) dropped {cb} (expected 0), refund -> {b} (expected true; entry survived default 1h TTL)"));
        rep("D71-c", cc > 0 && !c && cnt_c == 4, format!("charge r2 at T, compact(T+61m) dropped {cc} (expected >0), refund -> {c} (expected false), counter still {cnt_c} (expected 4: charge stands)"));
        rep("D71-d", cd > 0 && cnt_d == 10, format!("record r3 (5) at T, compact(T+61m) dropped {cd} (expected >0), record r3 again -> count {cnt_d} (expected 10: redelivery counted again)"));
    }
    // D72
    {
        let m = Meter::new(MemoryUsageStore::new(), vec![
            ("requests".into(), q(2, Window::Fixed{period: Duration::from_secs(60)})),
            ("requests".into(), q(100, Window::Fixed{period: Duration::from_secs(86400)})),
            ("input_tokens".into(), q(1000, Window::Fixed{period: Duration::from_secs(60)}))]).unwrap();
        let a = m.charge("q1","a","requests",1).await.unwrap();
        let b = m.charge("q2","a","requests",1).await.unwrap();
        let c = m.charge("q3","a","requests",1).await.unwrap();
        let o = m.charge("t1","a","input_tokens",500).await.unwrap();
        let un = m.charge("x","a","nosuch",1).await;
        rep("D72", a.is_allowed() && b.is_allowed() && c == Charge::Exceeded{used:2, limit:2} && o.is_allowed(), format!("{a:?} {b:?} third={c:?} other meter={o:?} unknown meter={:?}", un.err().map(|e| e.to_string())));
    }
    // D73
    for name in ["memory", "sqlite"] {
        let dir = std::env::temp_dir().join(format!("qa-h-d73-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (store, _): (Box<dyn UsageStore>, ()) = if name == "memory" { (Box::new(MemoryUsageStore::new()), ()) } else { (Box::new(SqliteUsageStore::open(dir.join("u.db")).unwrap()), ()) };
        let t = now();
        let k1 = key("a","m",3600,t); let k2 = MeterKey{ meter: "m".into(), identity:"a".into(), period_secs: 86400, window_start: t - t % 86400 };
        let op = OpId{ identity:"a".into(), meter:"m".into(), request_id:"op1".into(), phase: Phase::Reserve };
        let ra = store.apply(&op, &[Bound{key:&k1, limit:Some(10), retain_until:Some(t+7200)}, Bound{key:&k2, limit:Some(5), retain_until:Some(t+90000)}], 6).await.unwrap();
        let (g1, g2) = (store.get(&k1).await.unwrap(), store.get(&k2).await.unwrap());
        // first apply 3 on both OK
        let op0 = OpId{ request_id:"op0".into(), ..op.clone() };
        let r0 = store.apply(&op0, &[Bound{key:&k1, limit:Some(10), retain_until:Some(t+7200)}, Bound{key:&k2, limit:Some(5), retain_until:Some(t+90000)}], 3).await.unwrap();
        let rep1 = store.apply(&op0, &[Bound{key:&k1, limit:Some(10), retain_until:Some(t+7200)}, Bound{key:&k2, limit:Some(5), retain_until:Some(t+90000)}], 3).await.unwrap();
        let (h1, h2) = (store.get(&k1).await.unwrap(), store.get(&k2).await.unwrap());
        rep(&format!("D73-{name}"), matches!(ra, Apply::Rejected{index:1, ..}) && (g1,g2)==(0,0) && rep1 == r0 && (h1,h2)==(3,3),
            format!("apply 6 with bounds[10,5]={ra:?} keys=({g1},{g2}); apply op0 amount 3={r0:?}; repeat={rep1:?} keys=({h1},{h2})"));
    }
    // D74
    {
        let s = MemoryUsageStore::new();
        let dur = s.durability();
        let m = Meter::new(s, vec![("r".into(), q(10, Window::Fixed{period: Duration::from_secs(3600)}))]).unwrap();
        m.charge("c","a","r",4).await.unwrap();
        let c1 = m.store().get(&key("a","r",3600,now())).await.unwrap();
        drop(m);
        let s2 = MemoryUsageStore::new();
        let c2 = s2.get(&key("a","r",3600,now())).await.unwrap();
        rep("D74", dur == Durability::Volatile && c1 == 4 && c2 == 0 && s2.durability()==Durability::Volatile, format!("durability={dur:?} count before drop={c1} after recreate={c2}"));
    }
    println!("DONE-INPROC");
}
