use aide::{
	axum::{
		ApiRouter,
		routing::{get_with, post_with},
	},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Path, Query, State},
	http::StatusCode,
};
use chrono::{DateTime, FixedOffset};
use entities::{
	dm_delegates, group_members, group_message_reads, group_messages, groups,
	prelude::*,
	sea_orm_active_enums::{GroupKind, GroupMemberRole},
	user,
};
use schemars::JsonSchema;
use sea_orm::{
	ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, FromQueryResult, Order,
	QueryFilter, QueryOrder, QuerySelect, Set,
	sea_query::{Expr, Func, SelectStatement, SimpleExpr},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	api::{
		ApiState,
		v0::{
			groups::{
				ChatPlayer, GroupError, MAX_GROUP_MEMBERS, OwnPlayer, find_user_by_uuid,
				load_group, member_ids, require_membership,
			},
			social::{are_friends, is_blocked_either_way},
		},
	},
	utils::pagination::{Cursor, CursorPage, CursorQuery, Page, split_page},
};

/// Lower than the other v0 caps: each group costs a few queries to summarize.
const V0_LIST_CAP: u64 = 100;

#[derive(Debug, Serialize, JsonSchema)]
pub struct GroupSummary {
	id: i32,
	kind: GroupKind,
	name: Option<String>,
	members: Vec<Uuid>,
	last_message: Option<LastMessage>,
	unread: bool,
	/// An unclaimed Special Chat group (see GET /special-chat).
	special: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LastMessage {
	content: String,
	sender: Uuid,
	sent_at: DateTime<FixedOffset>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateGroupRequest {
	name: String,
	members: Vec<Uuid>,
}

fn dm_doc(op: TransformOperation) -> TransformOperation {
	op.id("openDirectMessage")
		.summary("Open (or fetch) a direct message with a friend")
		.description(
			"Returns the existing DM with the given player, or creates one. \
			 The two players must be friends and neither may have blocked the \
			 other.",
		)
		.tag("groups")
}

fn create_doc(op: TransformOperation) -> TransformOperation {
	op.id("createGroup")
		.summary("Create a group chat")
		.tag("groups")
}

fn list_doc(op: TransformOperation) -> TransformOperation {
	op.id("listGroups")
		.summary("List direct messages and group chats")
		.description(
			"Lists the 100 most recently active DMs and group chats the \
			 authenticated player belongs to.",
		)
		.tag("groups")
}

fn list_paged_doc(op: TransformOperation) -> TransformOperation {
	op.id("listGroupsPaged")
		.summary("List direct messages and group chats")
		.description(
			"Most recently active first. Pass the returned `cursor` back for the \
			 next page.",
		)
		.tag("groups")
}

fn claim_doc(op: TransformOperation) -> TransformOperation {
	op.id("claimSpecialChatGroup")
		.summary("Convert the Special Chat group into a normal, player-owned group chat")
		.description(
			"Claims ownership of the eagerly-created Special Chat group (see \
			 GET /special-chat), turning it into an ordinary group chat you \
			 own - a fresh Special Chat group is created the next time one is \
			 needed. Only works on that specific group.",
		)
		.tag("groups")
}

pub(super) fn router() -> ApiRouter<ApiState> {
	ApiRouter::new()
		.api_route("/dm/{player}", post_with(self::open_dm, self::dm_doc))
		.api_route("/", post_with(self::create_group, self::create_doc))
		.api_route("/", get_with(self::list, self::list_doc))
		.api_route("/{id}/claim", post_with(self::claim, self::claim_doc))
		.api_route(
			"/delegations",
			get_with(self::delegations, self::delegations_doc),
		)
}

pub(crate) fn v1_router() -> ApiRouter<ApiState> {
	ApiRouter::new().api_route("/", get_with(self::list_paged, self::list_paged_doc))
}

fn delegations_doc(op: TransformOperation) -> TransformOperation {
	op.summary("List players whose chats you can act as")
		.description(
			"Pass one of these as the `x-polyplus-act-as` header on chat routes.",
		)
		.tag("groups")
}

#[tracing::instrument(level = "debug", skip(state))]
async fn delegations(
	State(state): State<ApiState>,
	OwnPlayer(player): OwnPlayer,
) -> Result<Json<Vec<Uuid>>, GroupError> {
	let owner_ids = DmDelegates::find()
		.filter(dm_delegates::Column::DelegateId.eq(player.id))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|row| row.owner_id)
		.collect::<Vec<_>>();

	Ok(Json(
		User::find()
			.filter(user::Column::Id.is_in(owner_ids))
			.all(&state.database)
			.await?
			.into_iter()
			.map(|owner| owner.minecraft_uuid)
			.collect(),
	))
}

const PARTY_GROUP_NAME: &str = "Party";

async fn summarize(
	state: &ApiState,
	group: groups::Model,
	viewer_id: i32,
) -> Result<GroupSummary, GroupError> {
	let ids = member_ids(state, group.id).await?;
	let members = User::find()
		.filter(user::Column::Id.is_in(ids))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|user| user.minecraft_uuid)
		.collect();

	let last_message = GroupMessages::find()
		.filter(group_messages::Column::GroupId.eq(group.id))
		.filter(group_messages::Column::DeletedAt.is_null())
		.order_by_desc(group_messages::Column::SentAt)
		.one(&state.database)
		.await?;

	let mut unread = false;
	let last_message = match last_message {
		Some(message) => {
			let sender = User::find_by_id(message.sender_id)
				.one(&state.database)
				.await?;

			if message.sender_id != viewer_id {
				let read = GroupMessageReads::find()
					.filter(group_message_reads::Column::GroupId.eq(group.id))
					.filter(group_message_reads::Column::UserId.eq(viewer_id))
					.one(&state.database)
					.await?;
				let last_read = read.and_then(|read| read.last_read_message_id);
				unread = last_read.is_none_or(|last_read| last_read < message.id);
			}

			sender.map(|sender| LastMessage {
				content: message.content,
				sender: sender.minecraft_uuid,
				sent_at: message.sent_at,
			})
		}
		None => None,
	};

	Ok(GroupSummary {
		id: group.id,
		special: group.kind == GroupKind::Group
			&& group.owner_id.is_none()
			&& group.name.is_none(),
		name: display_name(&group.kind, group.name),
		kind: group.kind,
		members,
		last_message,
		unread,
	})
}

fn display_name(kind: &GroupKind, name: Option<String>) -> Option<String> {
	name.or_else(|| (*kind == GroupKind::Party).then(|| PARTY_GROUP_NAME.to_string()))
}

pub(crate) async fn get_or_create_dm_group(
	state: &ApiState,
	a: i32,
	b: i32,
) -> Result<(bool, groups::Model), GroupError> {
	let a_dm_groups = GroupMembers::find()
		.filter(group_members::Column::UserId.eq(a))
		.inner_join(Groups)
		.filter(groups::Column::Kind.eq(GroupKind::Dm))
		.all(&state.database)
		.await?
		.into_iter()
		.map(|member| member.group_id)
		.collect::<Vec<_>>();

	let existing = GroupMembers::find()
		.filter(group_members::Column::UserId.eq(b))
		.filter(group_members::Column::GroupId.is_in(a_dm_groups))
		.one(&state.database)
		.await?;

	if let Some(existing) = existing {
		return Ok((false, load_group(state, existing.group_id).await?));
	}

	let group = Groups::insert(groups::ActiveModel {
		kind: Set(GroupKind::Dm),
		name: Set(None),
		owner_id: Set(None),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	for member in [a, b] {
		GroupMembers::insert(group_members::ActiveModel {
			group_id: Set(group.id),
			user_id: Set(member),
			role: Set(GroupMemberRole::Member),
			joined_at: ActiveValue::NotSet,
		})
		.exec_without_returning(&state.database)
		.await?;
	}

	Ok((true, group))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn open_dm(
	State(state): State<ApiState>,
	OwnPlayer(player): OwnPlayer,
	Path(target): Path<Uuid>,
) -> Result<(StatusCode, Json<GroupSummary>), GroupError> {
	let target = find_user_by_uuid(&state, target).await?;
	if target.id == player.id {
		return Err(GroupError::SelfTarget);
	}
	if is_blocked_either_way(&state, player.id, target.id).await? {
		return Err(GroupError::Blocked);
	}
	if !are_friends(&state, player.id, target.id).await? {
		return Err(GroupError::NotFriends);
	}

	let (created, group) = get_or_create_dm_group(&state, player.id, target.id).await?;

	Ok((
		if created {
			StatusCode::CREATED
		} else {
			StatusCode::OK
		},
		Json(summarize(&state, group, player.id).await?),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn claim(
	State(state): State<ApiState>,
	ChatPlayer { player, .. }: ChatPlayer,
	Path(id): Path<i32>,
) -> Result<Json<GroupSummary>, GroupError> {
	if !state.special_chat_targets.contains(&player.minecraft_uuid) {
		return Err(GroupError::NotSpecialChatAccount);
	}

	require_membership(&state, id, player.id).await?;
	let group = load_group(&state, id).await?;
	if group.kind != GroupKind::Group || group.owner_id.is_some() || group.name.is_some()
	{
		return Err(GroupError::NotSpecialChatGroup);
	}

	let mut active: groups::ActiveModel = group.into();
	active.owner_id = Set(Some(player.id));
	let group = active.update(&state.database).await?;

	Ok(Json(summarize(&state, group, player.id).await?))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn create_group(
	State(state): State<ApiState>,
	OwnPlayer(player): OwnPlayer,
	Json(body): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<GroupSummary>), GroupError> {
	let mut member_ids = vec![player.id];
	for uuid in body.members {
		let user = find_user_by_uuid(&state, uuid).await?;
		if user.id == player.id {
			continue;
		}
		if is_blocked_either_way(&state, player.id, user.id).await? {
			return Err(GroupError::Blocked);
		}
		if !member_ids.contains(&user.id) {
			member_ids.push(user.id);
		}
	}

	if member_ids.len() > MAX_GROUP_MEMBERS {
		return Err(GroupError::GroupFull);
	}

	let group = Groups::insert(groups::ActiveModel {
		kind: Set(GroupKind::Group),
		name: Set(Some(body.name)),
		owner_id: Set(Some(player.id)),
		created_at: ActiveValue::NotSet,
		..Default::default()
	})
	.exec_with_returning(&state.database)
	.await?;

	for member in member_ids {
		GroupMembers::insert(group_members::ActiveModel {
			group_id: Set(group.id),
			user_id: Set(member),
			role: Set(if member == player.id {
				GroupMemberRole::Owner
			} else {
				GroupMemberRole::Member
			}),
			joined_at: ActiveValue::NotSet,
		})
		.exec_without_returning(&state.database)
		.await?;
	}

	Ok((
		StatusCode::CREATED,
		Json(summarize(&state, group, player.id).await?),
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list(
	State(state): State<ApiState>,
	ChatPlayer { player, .. }: ChatPlayer,
) -> Result<Json<Vec<GroupSummary>>, GroupError> {
	Ok(Json(
		load(&state, player.id, Page::first(V0_LIST_CAP))
			.await?
			.items,
	))
}

#[tracing::instrument(level = "debug", skip(state))]
async fn list_paged(
	State(state): State<ApiState>,
	ChatPlayer { player, .. }: ChatPlayer,
	Query(query): Query<CursorQuery>,
) -> Result<Json<CursorPage<GroupSummary>>, GroupError> {
	Ok(Json(load(&state, player.id, query.into()).await?))
}

#[derive(Debug, FromQueryResult)]
struct RankedGroup {
	id: i32,
	kind: GroupKind,
	name: Option<String>,
	owner_id: Option<i32>,
	created_at: DateTime<FixedOffset>,
	activity: i64,
}

/// Most recently active first, keyed on the newest message's id.
async fn load(
	state: &ApiState,
	player_id: i32,
	page: Page,
) -> Result<CursorPage<GroupSummary>, GroupError> {
	let newest_message = SelectStatement::new()
		.expr(Func::max(Expr::col((
			group_messages::Entity,
			group_messages::Column::Id,
		))))
		.from(group_messages::Entity)
		.and_where(
			Expr::col((group_messages::Entity, group_messages::Column::GroupId))
				.equals((groups::Entity, groups::Column::Id)),
		)
		.and_where(
			Expr::col((group_messages::Entity, group_messages::Column::DeletedAt))
				.is_null(),
		)
		.to_owned();
	let activity = SimpleExpr::from(Func::coalesce([
		SimpleExpr::SubQuery(None, Box::new(newest_message.into_sub_query_statement())),
		Expr::val(0_i64).into(),
	]));
	let group_id = SimpleExpr::from(Expr::col((groups::Entity, groups::Column::Id)));

	let mut select = Groups::find()
		.inner_join(GroupMembers)
		.filter(group_members::Column::UserId.eq(player_id))
		.expr_as(activity.clone(), "activity")
		.order_by(activity.clone(), Order::Desc)
		.order_by(group_id.clone(), Order::Desc)
		.limit(page.fetch_limit());
	if let Some(cursor) = page.cursor {
		select = select.filter(Expr::tuple([activity, group_id]).lt(Expr::tuple([
			Expr::val(cursor.rank).into(),
			Expr::val(cursor.id).into(),
		])));
	}

	let rows = select
		.into_model::<RankedGroup>()
		.all(&state.database)
		.await?;
	let (rows, cursor) = split_page(rows, page, |row| Cursor {
		rank: row.activity,
		id: row.id,
	});

	let mut items = Vec::with_capacity(rows.len());
	for row in rows {
		let group = groups::Model {
			id: row.id,
			kind: row.kind,
			name: row.name,
			owner_id: row.owner_id,
			created_at: row.created_at,
		};
		items.push(summarize(state, group, player_id).await?);
	}

	Ok(CursorPage { items, cursor })
}
