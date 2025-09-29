--() { :; }; exec psql -d local -h "$PWD/.local/postgresql" -f "$0"

INSERT INTO cosmetic (id, type, path)
VALUES
    (1, 'cape', 'cape.png'),
    (2, 'cape', 'cape2.png');

INSERT INTO cosmetic_package (package_id, cosmetic_id)
VALUES
    (6981022, 1),
    (6981115, 2);
