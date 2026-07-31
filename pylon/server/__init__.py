"""No Python code left here — `pylon serve` runs entirely in Rust now
(`pylon_server::run`, via the `pylon._core.run_server` binding). This
package still exists only as a home for `static/`, the built frontend
`pylon serve` serves at `/`; its location (`Path(__file__).parent /
"static"`) is computed in `pylon/cli/commands/serve.py` and passed to
`run_server` explicitly, since the Rust side has no notion of "where is
the installed `pylon` package" on its own.
"""
