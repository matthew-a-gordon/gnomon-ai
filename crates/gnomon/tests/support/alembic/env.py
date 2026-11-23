"""The env.py of a consumer's Alembic project, in the shape `alembic init`
writes, connecting to GNOMON_ALEMBIC_URL.

Like Alembic's own template, it runs every pending revision in one
transaction.
"""

import os

from alembic import context
from sqlalchemy import create_engine, pool

engine = create_engine(os.environ["GNOMON_ALEMBIC_URL"], poolclass=pool.NullPool)
with engine.connect() as connection:
    context.configure(connection=connection, target_metadata=None)
    with context.begin_transaction():
        context.run_migrations()
