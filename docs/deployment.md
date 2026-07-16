# Deployment guide

This guide covers installing artzain on a Linux server as a systemd-managed
foreground service. artzain itself stays a simple binary: systemd owns the
service lifecycle (start on boot, restart on failure, graceful stop), and a
reverse proxy fronts the app replicas.

## 1. Create the service account

Run artzain as an unprivileged user. The systemd unit hardens it further with
`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, and `PrivateTmp`.

```bash
sudo useradd --system --home /var/lib/artzain --shell /bin/false artzain
sudo mkdir -p /var/lib/artzain
sudo chown artzain:artzain /var/lib/artzain
sudo chmod 750 /var/lib/artzain
```

## 2. Install the binary

Download the release binary and place it somewhere on the service account's PATH
(or use the full path in the unit):

```bash
sudo curl -L -o /usr/local/bin/artzain \
  https://github.com/enekos/artzain/releases/latest/download/artzain-x86_64-unknown-linux-gnu
sudo chmod 755 /usr/local/bin/artzain
```

## 3. Place the manifest and apps

Put the `artzain.toml` and the prebuilt app binaries where the service account
can read them. The working directory in the unit defaults to the manifest's own
directory.

```bash
sudo mkdir -p /srv/app
sudo cp artzain.toml /srv/app/artzain.toml
sudo cp target/release/web /srv/app/web
sudo chown -R artzain:artzain /srv/app
```

## 4. Generate and install the systemd unit

From a privileged shell, generate the unit and install it:

```bash
sudo /usr/local/bin/artzain -f /srv/app/artzain.toml systemd \
  --user artzain --install
sudo systemctl daemon-reload
sudo systemctl enable --now artzain@default
```

This writes `/etc/systemd/system/artzain@.service`. The instance name
(`default` above) is arbitrary; you can run multiple fleets by enabling
`artzain@foo`, `artzain@bar`, etc. Each instance must use its own manifest and
port ranges.

### Unit options

- `--user NAME` — service account user (default: `artzain`).
- `--group NAME` — service account group (default: same as user).
- `--manifest PATH` — manifest path embedded in the unit (default: the `-f` file).
- `--binary PATH` — artzain binary path (default: `/usr/local/bin/artzain`).
- `--memory-max VALUE` — optional systemd `MemoryMax=` (e.g. `512M`, `2G`).
- `--install` — write the unit file instead of printing it.

The generated unit sets `KillSignal=SIGTERM` and `TimeoutStopSec=15`, giving
artzain's internal `STOP_GRACE` (10s) enough time to terminate child process
groups before systemd escalates.

## 5. Reverse-proxy the replicas

Each replica binds `port + i`. A reverse proxy should upstream to the whole
range.

### Caddy

```caddy
example.com {
    reverse_proxy localhost:8080 localhost:8081 localhost:8082
}
```

### nginx

```nginx
upstream web {
    server 127.0.0.1:8080;
    server 127.0.0.1:8081;
    server 127.0.0.1:8082;
}

server {
    listen 80;
    server_name example.com;

    location / {
        proxy_pass http://web;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### Apache

```apache
<Proxy "balancer://web">
    BalancerMember http://127.0.0.1:8080
    BalancerMember http://127.0.0.1:8081
    BalancerMember http://127.0.0.1:8082
</Proxy>

ProxyPreserveHost On
ProxyPass / balancer://web/
ProxyPassReverse / balancer://web/
```

## 6. Upgrade procedure

1. Drop the new binary (e.g. `sudo cp artzain /usr/local/bin/artzain`).
2. Edit the manifest (`/srv/app/artzain.toml`) to point at new app binaries or
   change env/replicas.
3. artzain watches the manifest and rolls the fleet automatically. For a
   controlled upgrade, restart the service so orphan reclamation runs:
   ```bash
   sudo systemctl restart artzain@default
   ```

## 7. Diagnosis

- `sudo systemctl status artzain@default` — service state and recent journal.
- `sudo journalctl -u artzain@default -f` — follow logs.
- `artzain -f /srv/app/artzain.toml status` — human-readable fleet state.
- `artzain -f /srv/app/artzain.toml status --check` — exit non-zero if not fully
  ready (useful for external health checks).
- `artzain -f /srv/app/artzain.toml status --json` — machine-readable state.
- `artzain -f /srv/app/artzain.toml logs --tail 100` — last 100 lines per log.
- `artzain -f /srv/app/artzain.toml logs -f` — follow logs as they grow.

## 8. Recovery runbook

### Stale lock / state after a crash

If `up` died without teardown (OOM, `kill -9`), the next `up` automatically
reclaims the orphaned child process groups and removes the stale lock/state on
startup. A manual `systemctl restart artzain@default` triggers this.

### Wedged app

If an app ignores SIGTERM, artzain escalates to SIGKILL after `STOP_GRACE`
(10s). You can also stop the whole fleet:

```bash
sudo systemctl stop artzain@default
```

### Disk-full from logs

Each instance's log is bounded by `[defaults].log_max_bytes` (default 10 MB) and
`log_keep` (default 3). If disk is still full, lower these values, remove old
`.artzain/logs/` files, and restart the service.

### Multiple fleets on one host

Use separate systemd instances with separate manifests and non-overlapping port
ranges:

```bash
sudo systemctl enable --now artzain@web
sudo systemctl enable --now artzain@worker
```

Each instance's unit file is the same template; the manifest path determines the
fleet.

## 9. File layout on the host

```
/usr/local/bin/artzain              # artzain binary
/etc/systemd/system/artzain@.service # unit template
/srv/app/artzain.toml               # fleet manifest
/srv/app/.artzain/                  # control plane
    state.json                      # live fleet snapshot (0600)
    lock                            # owner pid lock (0600)
    logs/                           # per-instance logs (0600)
/var/lib/artzain/                   # service account home
```
