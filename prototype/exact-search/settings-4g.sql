-- PROTOTYPE, wipe me. Settings for a 4 GiB managed plan.
alter system set shared_buffers = '1GB';
alter system set effective_cache_size = '3GB';
alter system set work_mem = '16MB';
alter system set maintenance_work_mem = '512MB';
alter system set max_connections = 50;
