"""The monthly re-bake on Modal. See "Running it monthly on Modal" in the README.

    modal run --detach modal_app.py::smoke   # Liechtenstein only, published under smoke-test/
    modal run --detach modal_app.py::bake    # a full bake and publish, now
    modal deploy modal_app.py                # the same full bake on the 23rd of every month
"""

import subprocess
import time

import modal

image = (
    modal.Image.debian_slim(python_version="3.12")
    .apt_install("build-essential", "cmake", "pkg-config", "curl", "ca-certificates", "unzip", "jq")
    .run_commands(
        "curl -fsSL https://rclone.org/install.sh | bash",
        "curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal",
    )
    .add_local_dir(
        ".", "/src", copy=True, ignore=["target", "work", "out", "cache", ".git", ".venv"]
    )
    .run_commands("cd /src && /root/.cargo/bin/cargo build --release --locked")
)

app = modal.App("arnis-tiles", image=image)
secrets = [modal.Secret.from_name("arnis-tiles-r2")]
TIMEOUT = 24 * 60 * 60


def bake_and_publish(**env: str) -> None:
    import os

    subprocess.run(
        ["bash", "/src/scripts/bake-and-publish.sh"],
        env={**os.environ, "DATA": "/bake", **env},
        check=True,
    )


# Four cores are reserved, and Modal lets the container burst to twenty. Baking decodes each
# extract on up to sixteen of them and peaks near 5.5 GB, mostly the largest extract's node
# coordinates. Writing each continent's archive runs on every core there is: on one core,
# Europe's and North America's took ten of the first run's thirteen hours, and here eleven
# minutes.
# Non-preemptible costs three times as much, but a preempted container restarts the whole planet
# from nothing.
#
# Not on the 1st: on 1 November 2026 Geofabrik gave each connection 0.3-0.6 MB/s, a tenth of what
# it served on 24 and 25 October, presumably because every monthly job downloads then. The 23rd
# is late enough to avoid that, and early enough that no month is too short for it; the odd
# minute steps around jobs that start on the hour.
@app.function(
    schedule=modal.Cron("41 3 23 * *", timezone="Europe/Berlin"),
    timeout=TIMEOUT,
    cpu=4.0,
    memory=16 * 1024,
    nonpreemptible=True,
    secrets=secrets,
)
def bake() -> None:
    bake_and_publish(DEADLINE=str(int(time.time()) + TIMEOUT))


# One-off: the 23 November 2026 run failed (Geofabrik had moved its files), and a rerun that
# afternoon met 0.8 MB/s downloads, so the bake runs again in the quiet of the next night. The
# schedule comes round every 24 November, but does nothing after 2026; delete this once it ran.
@app.function(schedule=modal.Cron("40 3 24 11 *", timezone="Europe/Berlin"), timeout=5 * 60)
def rerun_on_24_november_2026() -> None:
    import datetime

    if datetime.date.today().year == 2026:
        bake.spawn()


@app.function(timeout=60 * 60, cpu=1.0, memory=4 * 1024, secrets=secrets)
def smoke() -> None:
    bake_and_publish(SMOKE="1")
