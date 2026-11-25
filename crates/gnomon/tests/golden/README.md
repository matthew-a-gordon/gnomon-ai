# Released export output

These files are what `gnomon migrations export --embedding-dimension 1536`
writes for two migrations on the supabase and alembic targets, without the
Supabase version prefix. Consumers commit this output to their chains, and a
re-export refuses to run when a released file differs from what the release
would write. The test `released_supabase_and_alembic_output_never_changes` in
`tests/export.rs` compares the export with these files, so a change to the
text a target wraps around the SQL fails here before it reaches a consumer.

Never update these files to make that test pass. A change to a target's
output belongs in new migrations only.
