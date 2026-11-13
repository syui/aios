#!/usr/bin/sh
set -e
if [ "${1#-}" != "$1" ]; then
	set -- postgres "$@"
fi
if [ "$1" = postgres ] && [ ! -s "$PGDATA/PG_VERSION" ]; then
	auth=${POSTGRES_HOST_AUTH_METHOD:-scram-sha-256}
	if [ -z "$POSTGRES_PASSWORD" ] && [ "$auth" != trust ]; then
		echo "postgres: POSTGRES_PASSWORD is not set (or set POSTGRES_HOST_AUTH_METHOD=trust)" >&2
		exit 1
	fi
	user=${POSTGRES_USER:-postgres}
	db=${POSTGRES_DB:-$user}
	mkdir -p "$PGDATA"
	chmod 700 "$PGDATA"
	pw=/tmp/.pwfile.$$
	umask 077
	printf '%s\n' "$POSTGRES_PASSWORD" > "$pw"
	umask 022
	initdb --username="$user" --pwfile="$pw" --auth-local=trust --auth-host="$auth" --encoding=UTF8 --locale=C.UTF-8 -D "$PGDATA" $POSTGRES_INITDB_ARGS
	rm -f "$pw"
	printf 'host all all all %s\n' "$auth" >> "$PGDATA/pg_hba.conf"
	printf "listen_addresses = '*'\n" >> "$PGDATA/postgresql.conf"
	pg_ctl -D "$PGDATA" -o "-c listen_addresses= -c unix_socket_directories=/tmp" -w start
	if [ "$db" != postgres ]; then
		psql -v ON_ERROR_STOP=1 -h /tmp -U "$user" -d postgres -c "CREATE DATABASE \"$db\""
	fi
	for f in /docker-entrypoint-initdb.d/*; do
		case "$f" in
		*.sh) echo "postgres: running $f"; . "$f" ;;
		*.sql) echo "postgres: running $f"; psql -v ON_ERROR_STOP=1 -h /tmp -U "$user" -d "$db" -f "$f" ;;
		*) if [ -e "$f" ]; then echo "postgres: ignoring $f"; fi ;;
		esac
	done
	pg_ctl -D "$PGDATA" -m fast -w stop
	echo "postgres: init process complete"
fi
exec "$@"
