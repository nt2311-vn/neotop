use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(target_os = "linux")]
fn bench_classify(c: &mut Criterion) {
    use std::hint::black_box;
    let cases: &[(&str, Option<&str>)] = &[
        ("/usr/bin/java -jar /opt/app.jar --port 8080", None),
        ("node /srv/server.js", None),
        ("python3 -m uvicorn app:main", None),
        ("/usr/bin/dockerd --containerd /run/containerd.sock", None),
        (
            "/usr/bin/nginx: worker process",
            Some("0::/system.slice/docker-abc123.scope"),
        ),
        ("systemd --user", Some("0::/user.slice/user-1000.slice")),
        ("/usr/sbin/sshd: ubuntu@pts/0", None),
        (
            "qemu-system-x86_64 -enable-kvm -smp 4 -m 8192 -name guest=vm1",
            None,
        ),
        ("/usr/lib/firefox/firefox", None),
        ("bash -i", None),
    ];

    c.bench_function("classify_process_10x", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            for (cmd, cg) in cases {
                let s = neotop::bench_api::classify_to_debug(cmd, *cg);
                acc = acc.wrapping_add(s.len());
            }
            black_box(acc)
        });
    });
}

#[cfg(not(target_os = "linux"))]
fn bench_classify(_c: &mut Criterion) {}

criterion_group!(benches, bench_classify);
criterion_main!(benches);
