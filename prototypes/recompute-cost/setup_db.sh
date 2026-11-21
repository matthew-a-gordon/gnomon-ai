#!/usr/bin/env bash
# PROTOTYPE, wipe me. Create the base database with schema and functions. Usage: setup_db.sh <dbname>
set -euo pipefail
DB=${1:-gnomon_proto_base}; D=$(dirname "$0"); P="psql -h 127.0.0.1 -p 55432 -U postgres -v ON_ERROR_STOP=1 -q"
$P -d postgres -c "drop database if exists $DB" -c "create database $DB"
$P -d $DB -f $D/schema.sql -f $D/functions.sql
