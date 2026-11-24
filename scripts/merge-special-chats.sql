\set ON_ERROR_STOP on
BEGIN;

CREATE TEMP TABLE staff ON COMMIT DROP AS
SELECT id FROM "user" WHERE minecraft_uuid IN (:targets);

CREATE TEMP TABLE merge_plan ON COMMIT DROP AS
SELECT DISTINCT ON (src.id) src.id AS source_id, dst.id AS dest_id, p.user_id AS player_id
FROM groups src
JOIN group_members p ON p.group_id = src.id AND p.user_id NOT IN (SELECT id FROM staff)
JOIN group_members pd ON pd.user_id = p.user_id
JOIN groups dst ON dst.id = pd.group_id
	AND dst.kind = 'group' AND dst.name IS NULL
	AND dst.owner_id IN (SELECT id FROM staff)
WHERE src.kind = 'group' AND src.owner_id IS NULL AND src.name IS NULL
	AND NOT EXISTS (
		SELECT 1 FROM group_members o
		WHERE o.group_id = dst.id AND o.user_id <> p.user_id
			AND o.user_id NOT IN (SELECT id FROM staff)
	)
ORDER BY src.id, dst.id;

SELECT id FROM groups
WHERE id IN (SELECT source_id FROM merge_plan UNION SELECT dest_id FROM merge_plan)
ORDER BY id
FOR UPDATE;

SELECT u.minecraft_uuid AS player, m.source_id, m.dest_id,
	(SELECT count(*) FROM group_messages WHERE group_id = m.source_id) AS messages_moving
FROM merge_plan m JOIN "user" u ON u.id = m.player_id
ORDER BY m.player_id, m.source_id;

DELETE FROM group_messages gm
USING merge_plan m
WHERE gm.group_id = m.source_id
	AND gm.id = (SELECT min(id) FROM group_messages WHERE group_id = m.source_id)
	AND gm.sender_id IN (SELECT id FROM staff)
	AND EXISTS (
		SELECT 1 FROM group_messages d
		WHERE d.group_id = m.dest_id AND d.sender_id IN (SELECT id FROM staff)
			AND d.content = gm.content
	);

INSERT INTO group_members (group_id, user_id, role)
SELECT DISTINCT m.dest_id, sm.user_id, 'member'::group_member_role
FROM merge_plan m
JOIN group_members sm ON sm.group_id = m.source_id
WHERE NOT EXISTS (
	SELECT 1 FROM group_members e WHERE e.group_id = m.dest_id AND e.user_id = sm.user_id
);

UPDATE group_messages gm
SET group_id = m.dest_id, idempotency_key = NULL
FROM merge_plan m
WHERE gm.group_id = m.source_id;

DELETE FROM groups WHERE id IN (SELECT source_id FROM merge_plan);

\if :{?apply}
COMMIT;
\echo 'Applied.'
\else
ROLLBACK;
\echo 'Dry run, rolled back. Re-run with -v apply=1 to apply.'
\endif
