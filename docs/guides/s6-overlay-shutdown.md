# s6-overlay Kill-All Protocol and Prompt Container Shutdown

## What s6-overlay’s “kill-all” phase does

“Kill-all” is not the normal service-stop mechanism. It is the **final safety sweep** after s6-overlay has already tried to stop its registered services and run finish hooks.

The shutdown sequence is roughly:

1. Stop legacy `/etc/services.d` services.
2. Bring down native s6-rc services in reverse dependency order.
3. Run `/etc/cont-finish.d` scripts.
4. Send `SIGTERM` and `SIGCONT` to every remaining process in the container PID namespace.
5. Sleep for `S6_KILL_GRACETIME`.
6. Send `SIGKILL` to every remaining process.
7. Exit PID 1.

The underlying `s6-linux-init` implementation literally performs an unconditional:

```c
kill(-1, SIGTERM);
kill(-1, SIGCONT);
sleep(grace_time);
kill(-1, SIGKILL);
```

That sleep is not shortened when no processes remain. Consequently, the default `S6_KILL_GRACETIME=3000` adds approximately **three seconds to every ordinary container shutdown**, even when all supervised services stopped immediately.

`s6-overlay` passes `S6_KILL_GRACETIME` to `s6-linux-init-maker` as its final shutdown grace period.

## The practical shutdown strategy

The best model is:

> Gracefully stop known services through s6 supervision, give each service its own bounded timeout, and leave only a very small residual grace period for the final kill-all sweep.

Do not rely on the global kill-all phase as the normal way your application receives `SIGTERM`.

### 1. Prefer native s6-rc services

Native s6-rc services provide dependency-aware shutdown and per-service timeout controls. Independent services can stop concurrently, while dependent services stop in the correct order.

A native longrun might look like:

```text
/etc/s6-overlay/s6-rc.d/app/
├── type
├── run
├── down-signal
├── timeout-kill
├── timeout-finish
└── dependencies.d/
    └── base
```

`type`:

```text
longrun
```

`run`:

```sh
#!/command/execlineb -P

s6-setuidgid app
/usr/local/bin/my-app --foreground
```

`down-signal`:

```text
TERM
```

`timeout-kill`:

```text
3000
```

`timeout-finish`:

```text
1000
```

Then enable it:

```dockerfile
RUN touch /etc/s6-overlay/s6-rc.d/user/contents.d/app
```

The important properties are:

- The application runs in the foreground.
- The `run` script chain-loads or `exec`s the actual application.
- There is no `&`, daemonization, or long-lived wrapper shell.
- `down-signal` matches the application’s graceful-shutdown signal.
- `timeout-kill` prevents a nonresponsive application from blocking shutdown indefinitely.
- `timeout-finish` bounds any service `finish` script.

`s6-supervise` expects `run` to become the daemon rather than launch it in the background. It sends the configured down signal—`SIGTERM` by default—when taking the service down.

Without `timeout-kill`, an s6-rc longrun that ignores its stop signal can prevent its down transition from completing.

### 2. Handle child processes deliberately

By default, supervision directly controls the service’s main process. An application that spawns children must either:

- Reap and stop those children itself.
- Keep them in its process group and use `flag-timeout-killpg`.
- Clean up the process group in its `finish` script.

With current s6 versions, adding this empty file causes timeout escalation to target the service process group:

```text
/etc/s6-overlay/s6-rc.d/app/flag-timeout-killpg
```

A defensive `finish` script can also use the process-group argument supplied by s6:

```sh
#!/command/execlineb -P

foreground {
    s6-envuidgid app
    foreground { sleep 0.2 }
}
foreground {
    kill -KILL -- -${4}
}
```

In a shell script, the equivalent core operation is:

```sh
kill -KILL -- "-$4" 2>/dev/null || true
```

The fourth `finish` argument is the process-group ID. This is useful for leaked descendants, although it cannot catch a child that deliberately creates a different session or process group.

## Choosing `S6_KILL_GRACETIME`

Once every real service has a bounded service-level shutdown, reduce the final global delay:

```dockerfile
ENV S6_KILL_GRACETIME=250
```

A sensible range is usually **100–500 milliseconds**.

Setting it to zero is possible:

```dockerfile
ENV S6_KILL_GRACETIME=0
```

But that immediately follows the global `SIGTERM` with `SIGKILL`. Any unregistered helper, orphan, shell, or CMD process gets no chance to react. A small nonzero value is generally safer.

The key point is that raising `S6_KILL_GRACETIME` does not give properly supervised services more time. Those services should already have been stopped. It only gives **leftover processes from the final sweep** more time.

## CMD-based application versus supervised application

There are two valid arrangements, but they behave differently.

### Pattern A: the main application is an s6-rc service

This is the most deterministic setup for a multi-service container.

```dockerfile
ENTRYPOINT ["/init"]
```

Use:

```dockerfile
ENV S6_CMD_RECEIVE_SIGNALS=0
ENV S6_KILL_GRACETIME=250
```

The stop signal reaches PID 1, s6-overlay begins orderly teardown, and your application receives its configured `down-signal` during the service shutdown phase.

This is the recommended pattern when s6-overlay is actually supervising the application.

### Pattern B: the main application is `CMD`

For example:

```dockerfile
ENTRYPOINT ["/init"]
CMD ["/usr/local/bin/my-app"]
```

By default, the CMD process does **not** immediately receive Docker’s stop signal. It survives until the final process sweep and therefore only has `S6_KILL_GRACETIME` to shut down.

To make CMD receive the incoming signal immediately:

```dockerfile
ENV S6_CMD_RECEIVE_SIGNALS=1
```

When enabled, s6-overlay redirects supported signals such as `SIGTERM`, `SIGINT`, and `SIGQUIT` to CMD. s6-overlay does not begin the rest of its shutdown until CMD exits.

This gives the application a proper graceful-drain window, but it creates an important requirement:

> CMD must actually exit after receiving `SIGTERM`.

If it hangs, s6-overlay never reaches normal service teardown before the outer Docker or Kubernetes stop deadline expires.

Do not combine all three of these unless intentional:

```text
application is CMD
S6_CMD_RECEIVE_SIGNALS=0
S6_KILL_GRACETIME=250
```

That gives the CMD application only about 250 ms between `SIGTERM` and `SIGKILL`.

## Legacy `/etc/services.d` services

Legacy services use a different timeout:

```dockerfile
ENV S6_SERVICES_GRACETIME=2000
```

The default is 3000 ms. During shutdown, s6-overlay removes the legacy service links, requests that the services go down, and waits up to this global grace period.

`S6_SERVICES_GRACETIME` applies only to legacy `/etc/services.d` services. It is not the timeout for native s6-rc longruns.

Legacy services are workable, but native s6-rc is better when prompt shutdown matters because each service gets its own timeout and explicit dependency ordering.

## Finish-hook delays

There are two additional places shutdown can stall:

```dockerfile
ENV S6_KILL_FINISH_MAXTIME=1000
```

This bounds legacy `/etc/cont-finish.d` scripts. The default is 5000 ms.

For native longruns, use the service’s:

```text
timeout-finish
```

Keep finish scripts small. They should not perform lengthy uploads, retries, network calls, database migrations, or unbounded waits. Anything requiring durable completion belongs in the application’s normal shutdown path or in an external job system.

The overlay documents `S6_KILL_FINISH_MAXTIME`, `S6_SERVICES_GRACETIME`, and `S6_KILL_GRACETIME` as separate shutdown controls.

## Match the outer container stop timeout

Docker has its own deadline. `docker stop` sends the configured stop signal, normally `SIGTERM`, to PID 1 and sends `SIGKILL` when its timeout expires. The normal Linux default is ten seconds.

Compose exposes this as:

```yaml
services:
  app:
    stop_signal: SIGTERM
    stop_grace_period: 8s
```

`stop_grace_period` defaults to ten seconds when omitted.

The outer timeout needs to exceed:

```text
CMD drain time, when S6_CMD_RECEIVE_SIGNALS=1
+ longest service dependency-chain shutdown
+ finish/down script time
+ S6_KILL_GRACETIME
+ safety margin
```

Do not simply add every service timeout together: independent s6-rc services can stop concurrently. Add the time along the longest dependency chain.

A compact configuration for a well-behaved supervised application could be:

```yaml
services:
  app:
    image: my-app
    stop_signal: SIGTERM
    stop_grace_period: 8s
    environment:
      S6_KILL_GRACETIME: "250"
      S6_KILL_FINISH_MAXTIME: "1000"
```

For a CMD-based main application:

```yaml
services:
  app:
    image: my-app
    stop_signal: SIGTERM
    stop_grace_period: 8s
    environment:
      S6_CMD_RECEIVE_SIGNALS: "1"
      S6_KILL_GRACETIME: "250"
      S6_KILL_FINISH_MAXTIME: "1000"
```

## Diagnosing a slow stop

Measure the stop directly:

```sh
time docker stop --time 15 my-container
```

Temporarily enable detailed overlay logging:

```yaml
environment:
  S6_VERBOSITY: "3"
```

Inspect the process topology:

```sh
docker exec my-container \
  ps -eo pid,ppid,pgid,sid,stat,comm,args
```

Common timing signatures:

| Observed delay | Likely cause |
|---|---|
| Almost exactly 3 seconds every time | Default `S6_KILL_GRACETIME` fixed final sleep |
| Roughly 3 seconds around legacy service stop | `S6_SERVICES_GRACETIME` |
| Roughly 5 seconds around a finish hook | `S6_KILL_FINISH_MAXTIME` or `timeout-finish` |
| Almost exactly 10 seconds, then abrupt death | Docker’s outer stop deadline expired |
| Container never reaches finish hooks | CMD received the signal but did not exit under `S6_CMD_RECEIVE_SIGNALS=1` |
| Main process exits but children remain | Backgrounding, wrapper shell, separate process groups, or incomplete child cleanup |
| Native s6-rc shutdown stalls indefinitely | Missing or zero `timeout-kill` on a nonresponsive longrun |

For an intentional shutdown initiated inside the container, use:

```sh
/run/s6/basedir/bin/halt
```

Do not terminate the supervision tree directly with `s6-svscanctl -t`; s6-overlay v3 documents the generated `halt` command as the supported internal exit path.

## Recommended baseline

For most s6-overlay containers:

```text
Use native s6-rc for the application
run the application in the foreground
set down-signal explicitly
set timeout-kill per service
set timeout-finish per service
use process-group killing when the application spawns children
set S6_KILL_GRACETIME to around 250 ms
keep the outer stop deadline comfortably above the internal critical path
```

That moves graceful shutdown into the deterministic s6 service phase and reduces kill-all to what it should be: a brief cleanup pass for unexpected stragglers.

## Primary sources

- s6-overlay repository: `https://github.com/just-containers/s6-overlay`
- s6-overlay README: `https://github.com/just-containers/s6-overlay/blob/master/README.md`
- s6-overlay v3 migration notes: `https://github.com/just-containers/s6-overlay/blob/master/MOVING-TO-V3.md`
- s6 documentation: `https://skarnet.org/software/s6/`
- s6-rc documentation: `https://skarnet.org/software/s6-rc/`
- Docker container stop reference: `https://docs.docker.com/reference/cli/docker/container/stop/`
- Docker Compose services reference: `https://docs.docker.com/reference/compose-file/services/`
