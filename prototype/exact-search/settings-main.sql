-- PROTOTYPE, wipe me. Settings for the uncapped runs.
alter system set shared_buffers = '8GB';
alter system set work_mem = '64MB';
alter system set maintenance_work_mem = '2GB';
alter system set max_parallel_workers_per_gather = 0;
alter system set max_parallel_maintenance_workers = 8;
alter system set fsync = off;
alter system set full_page_writes = off;
alter system set synchronous_commit = off;
alter system set max_wal_size = '32GB';
alter system set checkpoint_timeout = '30min';
alter system set autovacuum = off;
alter system set jit = off;
alter system set effective_cache_size = '64GB';
alter system set random_page_cost = 1.1;
alter system set max_connections = 50;
