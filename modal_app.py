"""The monthly re-bake on Modal. See "Running it monthly on Modal" in the README.

    modal run --detach modal_app.py::smoke   # Liechtenstein only, published under smoke-test/
    modal run --detach modal_app.py::bake    # a full bake and publish, now
    modal deploy modal_app.py                # the same full bake on the 1st of every month
"""

import subprocess

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


def bake_and_publish(**env: str) -> None:
    import os

    subprocess.run(
        ["bash", "/src/scripts/bake-and-publish.sh"],
        env={**os.environ, "DATA": "/bake", **env},
        check=True,
    )


# The bake is one core plus a download thread and peaks near 5 GB. Non-preemptible costs three
# times as much, but a preempted container restarts the whole planet from nothing.
@app.function(
    schedule=modal.Cron("0 3 1 * *", timezone="Europe/Berlin"),
    timeout=24 * 60 * 60,
    cpu=1.0,
    memory=16 * 1024,
    nonpreemptible=True,
    secrets=secrets,
)
def bake() -> None:
    bake_and_publish()


@app.function(timeout=60 * 60, cpu=1.0, memory=4 * 1024, secrets=secrets)
def smoke() -> None:
    bake_and_publish(SMOKE="1")
