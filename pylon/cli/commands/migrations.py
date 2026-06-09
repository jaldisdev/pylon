import click

from ..config import requires_config


@click.group()
def migration() -> None:
    """Manage Pylon schema migrations."""


@migration.command()
@requires_config
def create() -> None:
    """Create a new migration file.

    Compares the current schema definition against the database state and
    generates a migration file for any detected changes. Exits with a message
    if the schema is already in sync.
    """
    click.echo("Comparing schema against database...")
    # TODO: diff schema definition vs database state, write migration file if changes detected


@migration.command("list")
@requires_config
def list_migrations() -> None:
    """List all available migration files."""
    click.echo("Listing migrations...")
    # TODO: scan migrations directory and print status table


@migration.command()
@click.option(
    "--dry-run",
    is_flag=True,
    default=False,
    help="Preview the SQL that would be executed without applying it.",
)
@requires_config
def migrate(dry_run: bool) -> None:
    """Apply all pending migrations."""
    if dry_run:
        click.echo("Dry run — no changes will be applied.")
    click.echo("Applying migrations...")
    # TODO: resolve pending migrations and execute via pylon runner
