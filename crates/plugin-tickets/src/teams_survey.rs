use super::*;

use std::collections::BTreeMap;

use closure_core::storage::MAX_VALUE_BYTES;

const DEFAULT_TEAM_ID: &str = "default";
const SURVEY_RESPONSE_PREFIX: &str = "ticket-exit-survey:";
const SURVEY_PROMPT_PREFIX: &str = "ticket-exit-survey-prompt:";
pub(super) const SURVEY_SUBMIT: &str = "survey-submit";
pub(super) const SURVEY_OPEN: &str = "survey-open";
pub(super) const SURVEY_VIEW: &str = "survey-view";
const MAX_SURVEY_CUSTOM_ID_BYTES: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub(super) struct StaffTeam {
    pub(super) id: String,
    pub(super) name: String,
    #[serde(default)]
    pub(super) roles: Vec<String>,
    #[serde(default)]
    pub(super) users: Vec<String>,
    #[serde(default)]
    pub(super) permissions: TeamPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TeamPermissions {
    #[serde(default = "enabled")]
    pub(super) add_reactions: bool,
    #[serde(default = "enabled")]
    pub(super) send_messages: bool,
    #[serde(default = "enabled")]
    pub(super) send_tts_messages: bool,
    #[serde(default = "enabled")]
    pub(super) embed_links: bool,
    #[serde(default = "enabled")]
    pub(super) attach_files: bool,
    pub(super) mention_everyone: bool,
    #[serde(default = "enabled")]
    pub(super) use_external_emojis: bool,
    #[serde(default = "enabled")]
    pub(super) use_application_commands: bool,
    #[serde(default = "enabled")]
    pub(super) use_external_stickers: bool,
    #[serde(default = "enabled")]
    pub(super) send_voice_messages: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct StaffAccessSnapshot {
    pub(super) teams: Vec<StaffTeamSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct StaffTeamSnapshot {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) roles: Vec<u64>,
    pub(super) users: Vec<u64>,
    pub(super) permissions: TeamPermissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SurveyRequest {
    channel: ChannelId,
    number: i64,
    attempt: i64,
}

impl Default for TeamPermissions {
    fn default() -> Self {
        Self {
            add_reactions: true,
            send_messages: true,
            send_tts_messages: true,
            embed_links: true,
            attach_files: true,
            mention_everyone: false,
            use_external_emojis: true,
            use_application_commands: true,
            use_external_stickers: true,
            send_voice_messages: true,
        }
    }
}

impl TeamPermissions {
    fn full() -> Self {
        Self {
            mention_everyone: true,
            ..Self::default()
        }
    }

    fn overwrite(&self) -> (Permissions, Permissions) {
        let bits = [
            (Permissions::ADD_REACTIONS, self.add_reactions),
            (Permissions::SEND_MESSAGES, self.send_messages),
            (Permissions::SEND_TTS_MESSAGES, self.send_tts_messages),
            (Permissions::EMBED_LINKS, self.embed_links),
            (Permissions::ATTACH_FILES, self.attach_files),
            (Permissions::MENTION_EVERYONE, self.mention_everyone),
            (Permissions::USE_EXTERNAL_EMOJIS, self.use_external_emojis),
            (
                Permissions::USE_SLASH_COMMANDS,
                self.use_application_commands,
            ),
            (
                Permissions::USE_EXTERNAL_STICKERS,
                self.use_external_stickers,
            ),
            (Permissions::SEND_VOICE_MESSAGES, self.send_voice_messages),
        ];
        bits.into_iter().fold(
            (Permissions::empty(), Permissions::empty()),
            |(allow, deny), (permission, enabled)| {
                if enabled {
                    (allow.union(permission), deny)
                } else {
                    (allow, deny.union(permission))
                }
            },
        )
    }

    fn overwrite_for_threads(&self) -> (Permissions, Permissions) {
        let (mut allow, mut deny) = self.overwrite();
        allow = allow.difference(Permissions::SEND_MESSAGES);
        deny = deny.difference(Permissions::SEND_MESSAGES);
        if self.send_messages {
            allow = allow.union(Permissions::SEND_MESSAGES_IN_THREADS);
        } else {
            deny = deny.union(Permissions::SEND_MESSAGES_IN_THREADS);
        }
        (allow, deny)
    }
}

fn enabled() -> bool {
    true
}

pub(super) fn valid_staff_teams(teams: &[StaffTeam]) -> bool {
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    teams.iter().all(|team| {
        let name = team.name.trim();
        let mut roles = HashSet::new();
        let mut users = HashSet::new();
        team.id != DEFAULT_TEAM_ID
            && !team.id.is_empty()
            && team.id.len() <= 32
            && team
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            && !name.is_empty()
            && name.chars().count() <= 80
            && !name.contains('\0')
            && ids.insert(team.id.as_str())
            && names.insert(name.to_lowercase())
            && team
                .roles
                .iter()
                .all(|id| id.parse::<u64>().is_ok_and(|id| id > 0) && roles.insert(id.as_str()))
            && team
                .users
                .iter()
                .all(|id| id.parse::<u64>().is_ok_and(|id| id > 0) && users.insert(id.as_str()))
    })
}

pub(super) fn valid_team_references(teams: &[StaffTeam], panel_refs: &[Vec<String>]) -> bool {
    if !valid_staff_teams(teams) {
        return false;
    }
    let known: HashSet<&str> = teams
        .iter()
        .map(|team| team.id.as_str())
        .chain([DEFAULT_TEAM_ID])
        .collect();
    panel_refs.iter().all(|references| {
        let mut seen = HashSet::new();
        references
            .iter()
            .all(|id| known.contains(id.as_str()) && seen.insert(id.as_str()))
    })
}

pub(super) fn staff_access_snapshot(
    teams: &[StaffTeam],
    selected: &[String],
    panel_staff_roles: &[RoleId],
    guild_id: u64,
) -> Result<Option<StaffAccessSnapshot>, UserError> {
    if selected.is_empty() {
        return Ok(None);
    }

    let references = [selected.to_vec()];
    if !valid_team_references(teams, &references) {
        return Err(UserError::message("tickets-team-invalid"));
    }

    let mut snapshots = Vec::with_capacity(selected.len());
    for id in selected {
        if id == DEFAULT_TEAM_ID {
            let mut roles = panel_staff_roles
                .iter()
                .map(|role| role.get())
                .collect::<Vec<_>>();
            if roles.contains(&guild_id) {
                return Err(UserError::message("tickets-team-invalid"));
            }
            roles.sort_unstable();
            roles.dedup();
            snapshots.push(StaffTeamSnapshot {
                id: DEFAULT_TEAM_ID.to_owned(),
                name: "Default".to_owned(),
                roles,
                users: Vec::new(),
                permissions: TeamPermissions::full(),
            });
            continue;
        }
        let team = teams
            .iter()
            .find(|team| team.id == *id)
            .ok_or_else(|| UserError::message("tickets-team-invalid"))?;
        let mut roles = team
            .roles
            .iter()
            .map(|role| {
                role.parse::<u64>()
                    .ok()
                    .filter(|role| *role != guild_id && *role > 0)
                    .ok_or_else(|| UserError::message("tickets-team-invalid"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut users = team
            .users
            .iter()
            .map(|user| {
                user.parse::<u64>()
                    .ok()
                    .filter(|user| *user > 0)
                    .ok_or_else(|| UserError::message("tickets-team-invalid"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        roles.sort_unstable();
        roles.dedup();
        users.sort_unstable();
        users.dedup();
        snapshots.push(StaffTeamSnapshot {
            id: team.id.clone(),
            name: team.name.clone(),
            roles,
            users,
            permissions: team.permissions.clone(),
        });
    }
    Ok(Some(StaffAccessSnapshot { teams: snapshots }))
}

pub(super) fn valid_thread_staff_team_configuration(config: &Config) -> bool {
    if !config
        .panels
        .iter()
        .any(|panel| !panel.staff_teams.is_empty())
    {
        return true;
    }

    type PermissionSet = Vec<(u64, Permissions, Permissions)>;
    type ThreadScope = (Vec<u64>, Vec<u64>, Option<(PermissionSet, PermissionSet)>);
    let mut scopes = BTreeMap::<u64, ThreadScope>::new();
    let mut check_scope = |parent: u64, selected: &[String], panel_role_text: &[String]| -> bool {
        let mut panel_staff = panel_role_text
            .iter()
            .filter_map(|role| super::id_of(Some(role)))
            .map(RoleId::new)
            .collect::<Vec<_>>();
        panel_staff.sort_unstable();
        panel_staff.dedup();
        let (mut roles, users, permission_scope) =
            match staff_access_snapshot(&config.staff_teams, selected, &panel_staff, 0) {
                Ok(Some(snapshot)) => {
                    let mut users = snapshot
                        .teams
                        .iter()
                        .flat_map(|team| team.users.iter().copied())
                        .collect::<Vec<_>>();
                    users.sort_unstable();
                    users.dedup();
                    let role_permissions = thread_role_overwrites(&snapshot)
                        .into_iter()
                        .map(|(role, allow, deny)| (role.get(), allow, deny))
                        .collect();
                    let user_permissions = user_overwrites(&snapshot)
                        .into_iter()
                        .map(|(user, allow, deny)| (user.get(), allow, deny))
                        .collect();
                    (
                        staff_roles(&snapshot),
                        users,
                        Some((role_permissions, user_permissions)),
                    )
                }
                Ok(None) => (
                    panel_staff.iter().map(|role| role.get()).collect(),
                    Vec::new(),
                    None,
                ),
                Err(_) => return false,
            };
        roles.sort_unstable();
        roles.dedup();
        match scopes.get_mut(&parent) {
            Some((existing_roles, existing_users, existing_permissions)) => {
                if existing_roles != &roles || existing_users != &users {
                    return false;
                }
                if let Some(permission_scope) = permission_scope {
                    if existing_permissions
                        .as_ref()
                        .is_some_and(|existing| existing != &permission_scope)
                    {
                        return false;
                    }
                    existing_permissions.get_or_insert(permission_scope);
                }
                true
            }
            None => {
                scopes.insert(parent, (roles, users, permission_scope));
                true
            }
        }
    };

    if let Some(parent) = super::id_of(config.thread_channel.as_ref())
        && !check_scope(parent, &[], &config.staff_roles)
    {
        return false;
    }
    config.panels.iter().all(|panel| {
        super::id_of(panel.thread_channel.as_ref())
            .is_none_or(|parent| check_scope(parent, &panel.staff_teams, &panel.staff_roles))
    })
}

pub(super) fn staff_roles(snapshot: &StaffAccessSnapshot) -> Vec<u64> {
    let mut roles = snapshot
        .teams
        .iter()
        .flat_map(|team| team.roles.iter().copied())
        .collect::<Vec<_>>();
    roles.sort_unstable();
    roles.dedup();
    roles
}

pub(super) fn may_access(
    snapshot: Option<&StaffAccessSnapshot>,
    legacy_roles: &[u64],
    actor_id: u64,
    actor_roles: &[RoleId],
    can_manage: bool,
) -> bool {
    can_manage
        || snapshot.map_or_else(
            || {
                actor_roles
                    .iter()
                    .any(|role| legacy_roles.contains(&role.get()))
            },
            |snapshot| {
                snapshot.teams.iter().any(|team| {
                    team.users.contains(&actor_id)
                        || actor_roles
                            .iter()
                            .any(|role| team.roles.contains(&role.get()))
                })
            },
        )
}

pub(super) fn role_overwrites(
    snapshot: &StaffAccessSnapshot,
) -> Vec<(RoleId, Permissions, Permissions)> {
    let mut combined = BTreeMap::<u64, (Permissions, Permissions)>::new();
    for team in &snapshot.teams {
        let (allow, deny) = team.permissions.overwrite();
        for role in &team.roles {
            let entry = combined
                .entry(*role)
                .or_insert((Permissions::empty(), Permissions::empty()));
            entry.0 = entry.0.union(allow);
            entry.1 = entry.1.union(deny);
            entry.1 = entry.1.difference(entry.0);
        }
    }
    combined
        .into_iter()
        .map(|(role, (allow, deny))| (RoleId::new(role), allow, deny))
        .collect()
}

pub(super) fn user_overwrites(
    snapshot: &StaffAccessSnapshot,
) -> Vec<(UserId, Permissions, Permissions)> {
    let mut combined = BTreeMap::<u64, (Permissions, Permissions)>::new();
    for team in &snapshot.teams {
        let (allow, deny) = team.permissions.overwrite();
        for user in &team.users {
            let entry = combined
                .entry(*user)
                .or_insert((Permissions::empty(), Permissions::empty()));
            entry.0 = entry.0.union(allow);
            entry.1 = entry.1.union(deny);
            entry.1 = entry.1.difference(entry.0);
        }
    }
    combined
        .into_iter()
        .map(|(user, (allow, deny))| (UserId::new(user), allow, deny))
        .collect()
}

pub(super) fn thread_role_overwrites(
    snapshot: &StaffAccessSnapshot,
) -> Vec<(RoleId, Permissions, Permissions)> {
    let mut combined = BTreeMap::<u64, (Permissions, Permissions)>::new();
    for team in &snapshot.teams {
        let (allow, deny) = team.permissions.overwrite_for_threads();
        for role in &team.roles {
            let entry = combined
                .entry(*role)
                .or_insert((Permissions::empty(), Permissions::empty()));
            entry.0 = entry.0.union(allow);
            entry.1 = entry.1.union(deny).difference(entry.0);
        }
    }
    combined
        .into_iter()
        .map(|(role, (allow, deny))| (RoleId::new(role), allow, deny))
        .collect()
}

pub(super) fn thread_member_overwrite(
    snapshot: &StaffAccessSnapshot,
    user_id: u64,
    roles: &[RoleId],
) -> (Permissions, Permissions) {
    let mut allow = Permissions::empty();
    let mut deny = Permissions::empty();
    for team in &snapshot.teams {
        if team.users.contains(&user_id)
            || roles.iter().any(|role| team.roles.contains(&role.get()))
        {
            let (team_allow, team_deny) = team.permissions.overwrite_for_threads();
            allow = allow.union(team_allow);
            deny = deny.union(team_deny);
        }
    }
    (allow, deny.difference(allow))
}

fn survey_questions(history: &TicketHistory) -> Option<&[advanced_forms::FormQuestion]> {
    let questions = &history.panel.as_ref()?.exit_survey;
    (!questions.is_empty() && advanced_forms::valid_questions(questions)).then_some(questions)
}

fn survey_response_key(request: SurveyRequest) -> String {
    format!(
        "{SURVEY_RESPONSE_PREFIX}{}:{}:{}",
        request.channel.get(),
        request.number,
        request.attempt
    )
}

fn survey_prompt_key(request: SurveyRequest) -> String {
    format!(
        "{SURVEY_PROMPT_PREFIX}{}:{}:{}",
        request.channel.get(),
        request.number,
        request.attempt
    )
}

fn survey_custom_id(
    route: &str,
    owner: Option<UserId>,
    module: &ModuleId,
    request: SurveyRequest,
) -> Option<String> {
    let data = format!(
        "{}:{}:{}",
        request.channel.get(),
        request.number,
        request.attempt
    );
    let mut id = CustomId::new(route);
    if let Some(owner) = owner {
        id = id.only_for(owner);
    }
    let encoded = id.data(data).encode(module);
    (encoded.len() <= MAX_SURVEY_CUSTOM_ID_BYTES).then_some(encoded)
}

fn survey_form_custom_id(
    owner: UserId,
    module: &ModuleId,
    request: SurveyRequest,
    revision: u32,
) -> Option<String> {
    let data = format!(
        "{}:{}:{}:{revision:08x}",
        request.channel.get(),
        request.number,
        request.attempt
    );
    let encoded = CustomId::new(SURVEY_SUBMIT)
        .only_for(owner)
        .data(data)
        .encode(module);
    (encoded.len() <= MAX_SURVEY_CUSTOM_ID_BYTES).then_some(encoded)
}

fn parse_survey_form_request(data: &str) -> Option<(SurveyRequest, u32)> {
    let mut parts = data.split(':');
    let (Some(channel), Some(number), Some(attempt), Some(revision), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    if revision.len() != 8 {
        return None;
    }
    let request = parse_survey_request(&format!("{channel}:{number}:{attempt}"))?;
    Some((request, u32::from_str_radix(revision, 16).ok()?))
}

fn survey_form_revision(questions: &[advanced_forms::FormQuestion]) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    questions.hash(&mut hasher);
    hasher.finish() as u32
}

fn parse_survey_request(data: &str) -> Option<SurveyRequest> {
    let mut parts = data.split(':');
    let (Some(channel), Some(number), Some(attempt), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let request = SurveyRequest {
        channel: ChannelId::new(channel.parse::<u64>().ok()?),
        number: number.parse().ok()?,
        attempt: attempt.parse().ok()?,
    };
    (request.channel.get() > 0 && request.number > 0 && request.attempt > 0).then_some(request)
}

async fn survey_history(
    ctx: &EventContext,
    request: SurveyRequest,
) -> Result<Option<(String, TicketHistory)>, StorageError> {
    let key = super::history_key(request.number, request.channel);
    let pointer = ctx
        .store()
        .get::<String>(&super::history_id_key(request.channel))
        .await?;
    if pointer.as_deref() != Some(key.as_str()) {
        return Ok(None);
    }
    let Some(entry) = ctx.store().entry(&key).await? else {
        return Ok(None);
    };
    let history = serde_json::from_value(entry.value)
        .map_err(|error| StorageError::Value(error.to_string()))?;
    Ok(Some((key, history)))
}

fn survey_history_matches(history: &TicketHistory, request: SurveyRequest, opener: u64) -> bool {
    history.version == 1
        && history.channel_id == request.channel.get()
        && history.number == request.number
        && history.attempt == request.attempt
        && history.status == HistoryStatus::Closed
        && !history.opener_erased
        && history.opener == opener
        && survey_questions(history).is_some()
}

async fn open_survey_modal(cx: &ComponentCx, request: SurveyRequest) -> Result<bool, UserError> {
    let ctx = cx.ctx();
    let Some((_, history)) = survey_history(&ctx, request).await.map_err(|error| {
        tracing::warn!(?error, channel = %request.channel, "could not read ticket exit survey");
        super::failed(&error)
    })?
    else {
        return Ok(false);
    };
    if !survey_history_matches(&history, request, cx.user().id.get()) {
        return Ok(false);
    }
    let response = ctx
        .store()
        .get::<Vec<(String, String)>>(&survey_response_key(request))
        .await
        .map_err(|error| {
            tracing::warn!(?error, channel = %request.channel, "could not check ticket exit survey state");
            super::failed(&error)
        })?;
    if let Some(answers) = response {
        if !answers.is_empty() {
            if let Err(error) = super::update_exit_survey_archive(&ctx, &history).await {
                tracing::warn!(?error, channel = %request.channel, "could not update ticket exit survey archive");
            }
        }
        return Ok(false);
    }

    let Some(questions) = survey_questions(&history) else {
        return Ok(false);
    };
    let resolved_questions = advanced_forms::resolve_form_questions(
        ctx,
        questions,
        &history.survey_integrations,
        ctx.guild_id.get(),
    )
    .await?;
    if !advanced_forms::valid_questions(&resolved_questions) {
        return Err(UserError::message("tickets-form-invalid"));
    }
    let custom_id = survey_form_custom_id(
        cx.user().id,
        cx.module(),
        request,
        survey_form_revision(&resolved_questions),
    )
    .ok_or_else(|| UserError::message("tickets-exit-survey-stale"))?;
    let inputs = resolved_questions
        .iter()
        .enumerate()
        .map(|(index, question)| question.component(cx.module(), index));
    let modal = ui::modal(
        custom_id,
        cx.t("tickets-exit-survey-title", &Args::new()),
        inputs,
    );
    cx.show_modal(modal).await?;
    Ok(true)
}

pub(super) async fn maybe_open_exit_survey(
    cx: &ComponentCx,
    channel: ChannelId,
    number: i64,
    attempt: i64,
) -> Result<bool, UserError> {
    open_survey_modal(
        cx,
        SurveyRequest {
            channel,
            number,
            attempt,
        },
    )
    .await
}

pub(super) async fn open_exit_survey_button(cx: ComponentCx) -> Result<(), UserError> {
    let Some(custom_id) = cx.custom_id() else {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    };
    if custom_id.route != SURVEY_OPEN || custom_id.owner != Some(cx.user().id) {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }
    let Some(request) = parse_survey_request(&custom_id.data) else {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    };
    if !open_survey_modal(&cx, request).await? {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }
    Ok(())
}

pub(super) async fn survey_command(cx: CommandCx, number: i64) -> Result<(), UserError> {
    if number <= 0 {
        return cx
            .reply_private_t("tickets-exit-survey-stale", &Args::new())
            .await;
    }
    let ctx = cx.ctx();
    let history = match super::history_by_number(&ctx, number).await {
        Ok(Some(history)) => history,
        Ok(None) => {
            return cx
                .reply_private_t("tickets-exit-survey-stale", &Args::new())
                .await;
        }
        Err(error) => return Err(super::failed(&error)),
    };
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    if history.opener != cx.user().id.get() || !survey_history_matches_closed(&history, request) {
        return cx
            .reply_private_t("tickets-exit-survey-stale", &Args::new())
            .await;
    }
    if !data::history_privacy_generation_current(
        &ctx,
        &super::history_key(number, request.channel),
        history.privacy_generation,
    )
    .await
    .map_err(|error| super::failed(&error))?
    {
        return cx
            .reply_private_t("tickets-exit-survey-stale", &Args::new())
            .await;
    }
    if ctx
        .store()
        .get::<Vec<(String, String)>>(&survey_response_key(request))
        .await
        .map_err(|error| super::failed(&error))?
        .is_some()
    {
        return cx
            .reply_private_t(
                "tickets-exit-survey-already",
                &Args::new().with("number", number),
            )
            .await;
    }
    let Some(button) = survey_open_component(&ctx, &history) else {
        return cx
            .reply_private_t("tickets-exit-survey-stale", &Args::new())
            .await;
    };
    cx.reply_with(InteractionReply {
        content: Some(cx.t(
            "tickets-exit-survey-prompt",
            &Args::new().with("number", number),
        )),
        components: vec![ui::row(vec![button])],
        ephemeral: true,
        ..InteractionReply::default()
    })
    .await
}

pub(super) fn survey_open_component(
    ctx: &EventContext,
    history: &TicketHistory,
) -> Option<Component> {
    if history.status != HistoryStatus::Closed
        || history.opener_erased
        || survey_questions(history).is_none()
    {
        return None;
    }
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    let custom_id = survey_custom_id(
        SURVEY_OPEN,
        Some(UserId::new(history.opener)),
        &ctx.module,
        request,
    )?;
    Some(ui::button(
        ButtonStyle::Primary,
        ctx.t_in(ctx.locale, "tickets-exit-survey-open", &Args::new()),
        custom_id,
    ))
}

async fn claim_exit_survey_prompt(
    ctx: &EventContext,
    history: &TicketHistory,
) -> Result<bool, ModuleError> {
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    let ttl = super::history_ttl(history, ctx.now())?;
    if ttl.is_zero() {
        return Ok(false);
    }
    match ctx
        .store()
        .replace(
            &survey_prompt_key(request),
            &true,
            Expect::Missing,
            Some(ttl),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(StorageError::Conflict) => Ok(false),
        Err(error) => Err(super::stored(&error)),
    }
}

pub(super) async fn send_exit_survey_prompt(ctx: &EventContext, history: &TicketHistory) -> bool {
    if history.opener_erased || survey_questions(history).is_none() {
        return false;
    }
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    let current = match survey_history(ctx, request).await {
        Ok(Some((_, current))) => current,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not read ticket exit survey prompt state");
            return false;
        }
    };
    if current.opener != history.opener || !survey_history_matches_closed(&current, request) {
        return false;
    }
    match ctx
        .store()
        .get::<Vec<(String, String)>>(&survey_response_key(request))
        .await
    {
        Ok(Some(_)) => return false,
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not check ticket exit survey state");
            return false;
        }
    }
    let Some(button) = survey_open_component(ctx, &current) else {
        return false;
    };
    match claim_exit_survey_prompt(ctx, &current).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not claim the ticket exit survey prompt");
            return false;
        }
    }
    match ctx
        .api
        .execute(Action::SendDirectMessage {
            guild_id: ctx.guild_id,
            user_id: UserId::new(current.opener),
            content: Some(ctx.t_in(
                ctx.locale,
                "tickets-exit-survey-prompt",
                &Args::new().with("number", current.number),
            )),
            embeds: Vec::new(),
            components: vec![ui::row(vec![button])],
            files: Vec::new(),
        })
        .await
    {
        Ok(Outcome::Message(_)) => true,
        Ok(_) => {
            tracing::warn!(channel = %request.channel, "ticket exit survey prompt returned no message");
            false
        }
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not send the ticket exit survey prompt");
            false
        }
    }
}

pub(super) async fn submit_exit_survey(cx: ModalCx) -> Result<(), UserError> {
    let Some((module, custom_id)) = CustomId::parse(&cx.submission().custom_id) else {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    };
    if &module != cx.module()
        || custom_id.route != SURVEY_SUBMIT
        || custom_id.owner != Some(cx.user().id)
    {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }
    let Some((request, form_revision)) = parse_survey_form_request(&custom_id.data) else {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    };
    let ctx = cx.ctx();
    let Some((history_key, history)) = survey_history(&ctx, request).await.map_err(|error| {
        tracing::warn!(?error, channel = %request.channel, "could not read ticket exit survey");
        super::failed(&error)
    })?
    else {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    };
    if !survey_history_matches(&history, request, cx.user().id.get()) {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }
    if !data::history_privacy_generation_current(
        &ctx,
        &history_key,
        history.privacy_generation,
    )
    .await
    .map_err(|error| {
        tracing::warn!(?error, channel = %request.channel, "could not verify ticket exit survey privacy state");
        super::failed(&error)
    })? {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }

    let Some(raw_questions) = survey_questions(&history) else {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    };
    cx.defer(true).await?;
    let questions = advanced_forms::resolve_form_questions_fresh(
        ctx,
        raw_questions,
        &history.survey_integrations,
        ctx.guild_id.get(),
    )
    .await?;
    if !advanced_forms::valid_questions(&questions) {
        return Err(UserError::message("tickets-form-invalid"));
    }
    if survey_form_revision(&questions) != form_revision {
        return say(&cx, "tickets-form-changed", &Args::new()).await;
    }
    let mut answers = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        match question.answer(cx.value(&format!("question-{index}"))) {
            Ok(Some(answer)) => answers.push(answer),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }
    if !advanced_forms::valid_form_answers(&questions, &answers)
        || serde_json::to_vec(&answers)
            .map_err(|_| UserError::message("tickets-form-invalid"))?
            .len()
            > MAX_VALUE_BYTES
    {
        return Err(UserError::message("tickets-form-invalid"));
    }

    let ttl = super::history_ttl(&history, cx.now())
        .map_err(|_| UserError::message("tickets-exit-survey-stale"))?;
    if ttl.is_zero() {
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }
    let response_key = survey_response_key(request);
    match ctx
        .store()
        .replace(&response_key, &answers, Expect::Missing, Some(ttl))
        .await
    {
        Ok(_) => {}
        Err(StorageError::Conflict) => {
            if !answers.is_empty() {
                if let Err(error) = super::update_exit_survey_archive(&ctx, &history).await {
                    tracing::warn!(?error, channel = %request.channel, "could not update ticket exit survey archive");
                }
            }
            return cx
                .reply_private_t(
                    "tickets-exit-survey-already",
                    &Args::new().with("number", request.number),
                )
                .await;
        }
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not save ticket exit survey");
            return Err(super::failed(&error));
        }
    }

    let still_current = match survey_history(&ctx, request).await {
        Ok(Some((key, current))) => {
            let privacy_current = match data::history_privacy_generation_current(
                &ctx,
                &history_key,
                history.privacy_generation,
            )
            .await
            {
                Ok(current) => current,
                Err(error) => {
                    tracing::warn!(?error, channel = %request.channel, "could not recheck ticket exit survey privacy state");
                    false
                }
            };
            key == history_key
                && survey_history_matches(&current, request, cx.user().id.get())
                && current.privacy_generation == history.privacy_generation
                && privacy_current
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not recheck ticket exit survey privacy state");
            false
        }
    };
    if !still_current {
        let _ = ctx.store().delete(&response_key).await;
        return say(&cx, "tickets-exit-survey-stale", &Args::new()).await;
    }
    if !answers.is_empty()
        && let Err(error) = super::update_exit_survey_archive(&ctx, &history).await
    {
        tracing::warn!(?error, channel = %request.channel, "could not update ticket exit survey archive");
    }

    cx.reply_private_t(
        "tickets-exit-survey-submitted",
        &Args::new().with("number", request.number),
    )
    .await
}

pub(super) fn survey_view_components(
    ctx: &EventContext,
    history: &TicketHistory,
) -> Vec<Component> {
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    let Some(custom_id) = survey_custom_id(SURVEY_VIEW, None, &ctx.module, request) else {
        return Vec::new();
    };
    vec![ui::row(vec![ui::button(
        ButtonStyle::Secondary,
        ctx.t_in(ctx.locale, "tickets-exit-survey-view", &Args::new()),
        custom_id,
    )])]
}

pub(super) async fn view_exit_survey(cx: ComponentCx) -> Result<(), UserError> {
    let Some(custom_id) = cx.custom_id() else {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    };
    if custom_id.route != SURVEY_VIEW || custom_id.owner.is_some() {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    }
    let Some(request) = parse_survey_request(&custom_id.data) else {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    };
    let ctx = cx.ctx();
    let Some((history_key, history)) = survey_history(&ctx, request).await.map_err(|error| {
        tracing::warn!(?error, channel = %request.channel, "could not read ticket exit survey history");
        super::failed(&error)
    })?
    else {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    };
    let roles = cx.actor().await.roles;
    let can_manage = cx.require("tickets.manage").await.is_ok();
    if !survey_history_matches_closed(&history, request)
        || !may_access(
            history.staff_access.as_ref(),
            &history.staff_roles,
            cx.user().id.get(),
            &roles,
            can_manage,
        )
    {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    }
    let privacy_current = data::history_privacy_generation_current(
        &ctx,
        &history_key,
        history.privacy_generation,
    )
    .await
    .map_err(|error| {
        tracing::warn!(?error, channel = %request.channel, "could not verify ticket exit survey privacy state");
        super::failed(&error)
    })?;
    if !privacy_current {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    }
    let Some(answers) = ctx
        .store()
        .get::<Vec<(String, String)>>(&survey_response_key(request))
        .await
        .map_err(|error| {
            tracing::warn!(?error, channel = %request.channel, "could not load ticket exit survey answers");
            super::failed(&error)
        })?
        .filter(|answers| !answers.is_empty())
    else {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    };
    if !data::history_privacy_generation_current(
        &ctx,
        &history_key,
        history.privacy_generation,
    )
    .await
    .map_err(|error| {
        tracing::warn!(?error, channel = %request.channel, "could not recheck ticket exit survey privacy state");
        super::failed(&error)
    })? {
        return say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await;
    }
    let embeds = advanced_forms::form_answers_embeds_in(&ctx, &answers)
        .map_err(|_| UserError::message("tickets-exit-survey-view-failed"))?;
    match ctx
        .api
        .execute(Action::SendDirectMessage {
            guild_id: ctx.guild_id,
            user_id: cx.user().id,
            content: None,
            embeds,
            components: Vec::new(),
            files: Vec::new(),
        })
        .await
    {
        Ok(Outcome::Message(_)) => {
            cx.reply_private_t(
                "tickets-exit-survey-view-sent",
                &Args::new().with("number", request.number),
            )
            .await
        }
        Ok(_) => say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await,
        Err(error) => {
            tracing::warn!(?error, channel = %request.channel, "could not DM ticket exit survey answers");
            say(&cx, "tickets-exit-survey-view-failed", &Args::new()).await
        }
    }
}

fn survey_history_matches_closed(history: &TicketHistory, request: SurveyRequest) -> bool {
    history.version == 1
        && history.channel_id == request.channel.get()
        && history.number == request.number
        && history.attempt == request.attempt
        && history.status == HistoryStatus::Closed
        && !history.opener_erased
        && survey_questions(history).is_some()
}

pub(super) async fn delete_exit_survey(
    ctx: &EventContext,
    history: &TicketHistory,
) -> Result<(), StorageError> {
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    ctx.store().delete(&survey_response_key(request)).await?;
    ctx.store()
        .delete(&survey_prompt_key(request))
        .await
        .map(|_| ())
}

pub(super) async fn exit_survey_answers_for_export(
    ctx: &EventContext,
    history: &TicketHistory,
    user: UserId,
) -> Result<Option<Vec<(String, String)>>, StorageError> {
    if history.opener_erased || history.opener != user.get() {
        return Ok(None);
    }
    let request = SurveyRequest {
        channel: ChannelId::new(history.channel_id),
        number: history.number,
        attempt: history.attempt,
    };
    Ok(ctx
        .store()
        .get::<Vec<(String, String)>>(&survey_response_key(request))
        .await?
        .filter(|answers| !answers.is_empty()))
}
