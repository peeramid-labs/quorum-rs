# About the agent working-directory override

How a middleware can choose the directory an agent's subprocess runs in, and what a
custom agent should do with it. This is a generic SDK feature — it knows nothing
about what a middleware puts in that directory.

## The idea

Agents that shell out (e.g. a CLI-backed agent) run in some working directory. By
default that's the agent's configured `working_dir`, or — if unset — wherever the
worker process was launched.

Sometimes a middleware needs the agent to run somewhere else: a directory it
prepared for *this specific task*. So the SDK lets a `before_prompt` middleware
**declare a working directory**, and the runtime starts the agent there.

## How it works

Middleware is loaded as a dynamic library implementing a simple JSON interface: at
each stage it receives a context and returns a verdict. When a `before_prompt`
middleware returns its (possibly transformed) content, that content may include a
directory path under the key `agent_working_dir`. The worker copies it into a runtime-only
field:

```rust
pub working_dir_override: Option<PathBuf>,
```

The agent then runs with its working directory resolved in this order (first one set
wins):

1. `working_dir_override` — the per-task directory the middleware declared
2. the agent's configured `working_dir`
3. the process launch directory

For a CLI-backed agent the override drives the subprocess's working directory, any
session-recovery file lookups, and how relative paths (like configured context
files) are resolved — so everything the subprocess does lands in the same place.

## What SDK users do

- **Writing a middleware:** return a directory path if you want the agent to run in
  a per-task location you set up. It's optional — omit it and agents use their static
  `working_dir`. It's a *request*: the runtime decides the working directory from the
  order above.
- **Writing a custom agent:** if your agent shells out or reads files, resolve its
  working directory as `working_dir_override` → configured `working_dir` → process
  cwd, and use it for both the subprocess and any relative-path resolution. The
  reference CLI agent's `phase_working_dir` is the pattern to copy.
- **Everyone else:** nothing to do. The field is `None` unless a middleware sets it,
  and behaviour is unchanged.

## Why not just static config?

A configured `working_dir` is one fixed directory per agent. A long-lived worker
handles many tasks, and a middleware may want a *different* directory per task —
created at runtime, so its path isn't known at config time. The override binds the
working directory to whatever the middleware prepared for the current task, while the
static field stays the sensible default when no middleware sets one.

See [the middleware system](middleware.md) for the dynamic-library interface and the
`before_prompt` / `provider_response` stages.
