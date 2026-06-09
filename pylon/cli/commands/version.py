import click


@click.command()
def version() -> None:
    """Show the current Pylon library version."""
    try:
        from importlib.metadata import version as _version

        v = _version("pylon")
    except Exception:
        v = "(development)"
    click.echo(f"Pylon {v}")
