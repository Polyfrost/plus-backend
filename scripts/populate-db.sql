--() { :; }; exec psql -d local -h "$PWD/.local/postgresql" -f "$0"

-- Wyvest (https://api.mojang.com/users/profiles/minecraft/Wyvest)
\set wyvest_uuid 'a5331404-0e77-440e-8bef-24c071dac1ae'

TRUNCATE user_cosmetic, cosmetic_package, cosmetic RESTART IDENTITY CASCADE;

INSERT INTO cosmetic (id, type, path)
VALUES
    (1, 'cape', 'capes/oneclient.png'),
    (2, 'cape', 'capes/oneconfig.png'),
    (3, 'cape', 'capes/onelauncher.png'),
    (4, 'cape', 'capes/poly.png'),
    (5, 'cape', 'capes/moon.png'),
    (6, 'emote', 'emotes/player.zip'),
    (7, 'emote', 'emotes/wowtext.zip'),
    (8, 'emote', 'emotes/santaguise.zip');

SELECT setval(
    pg_get_serial_sequence('cosmetic', 'id'),
    (SELECT COALESCE(MAX(id), 1) FROM cosmetic)
);

INSERT INTO "user" (minecraft_uuid)
VALUES (:'wyvest_uuid'::uuid)
ON CONFLICT (minecraft_uuid) DO NOTHING;

INSERT INTO user_cosmetic ("user", cosmetic, active)
SELECT
    u.id,
    c.id,
    (c.id = 1 AND c.type = 'cape') OR (c.id = 6 AND c.type = 'emote')
FROM "user" u
CROSS JOIN cosmetic c
WHERE u.minecraft_uuid = :'wyvest_uuid'::uuid;
