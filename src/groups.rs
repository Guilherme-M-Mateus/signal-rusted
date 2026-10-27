//! Group management (GroupsV2): who may change what, and the change actions sent to the
//! groups server. The network side lives in `backend.rs` (and vendored presage).

use presage::libsignal_service::groups_v2::{AccessRequired, GroupDecodingError, GroupMemberCandidate, GroupOperations, Role};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::proto::group_change::{Actions, actions as a};
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::libsignal_service::zkgroup::ServerPublicParams;
use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
use presage::libsignal_service::prelude::Timer;

/// What the group info panel shows; `None` on 1:1 conversations.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupInfo {
    pub description: String,
    /// (ACI, is admin)
    pub members: Vec<(Uuid, bool)>,
    /// Invited, not yet accepted.
    pub pending: usize,
    /// Asked to join via link, awaiting an admin.
    pub requesting: usize,
    /// Who may change title, description and timer.
    pub attributes: AccessRequired,
    /// Who may add members.
    pub add_members: AccessRequired,
}

impl GroupInfo {
    pub fn from_group(g: &presage::model::groups::Group) -> Self {
        let access = g.access_control.as_ref();
        GroupInfo {
            description: g.description.clone().unwrap_or_default(),
            members: g
                .members
                .iter()
                .map(|m| (ServiceId::from(m.aci).raw_uuid(), m.role == Role::Administrator))
                .collect(),
            pending: g.pending_members.len(),
            requesting: g.requesting_members.len(),
            attributes: access.map_or(AccessRequired::Member, |a| a.attributes),
            add_members: access.map_or(AccessRequired::Member, |a| a.members),
        }
    }

    pub fn is_member(&self, id: Uuid) -> bool {
        self.members.iter().any(|(m, _)| *m == id)
    }

    pub fn is_admin(&self, id: Uuid) -> bool {
        self.members.iter().any(|(m, admin)| *m == id && *admin)
    }

    fn allows(&self, access: AccessRequired, me: Uuid) -> bool {
        match access {
            AccessRequired::Any | AccessRequired::Member => self.is_member(me),
            AccessRequired::Administrator => self.is_admin(me),
            AccessRequired::Unknown | AccessRequired::Unsatisfiable => false,
        }
    }

    pub fn can_edit_details(&self, me: Uuid) -> bool {
        self.allows(self.attributes, me)
    }

    pub fn can_add_members(&self, me: Uuid) -> bool {
        self.allows(self.add_members, me)
    }

    /// Signal won't let the last admin leave without appointing another one first.
    pub fn must_appoint_admin(&self, me: Uuid) -> bool {
        self.is_admin(me) && self.members.iter().filter(|(_, admin)| *admin).count() == 1 && self.members.len() > 1
    }

    /// Whether `me` may make this change, given our role and the group's access control.
    pub fn allows_edit(&self, me: Uuid, edit: &GroupEdit) -> bool {
        match edit {
            GroupEdit::Title(t) => !t.trim().is_empty() && self.can_edit_details(me),
            GroupEdit::Description(_) | GroupEdit::Timer(_) => self.can_edit_details(me),
            GroupEdit::AddMembers(ids) => {
                !ids.is_empty() && ids.iter().all(|id| !self.is_member(*id)) && self.can_add_members(me)
            }
            GroupEdit::Remove(id) => *id != me && self.is_member(*id) && self.is_admin(me),
            GroupEdit::SetAdmin(id, admin) => {
                *id != me && self.is_member(*id) && self.is_admin(*id) != *admin && self.is_admin(me)
            }
            GroupEdit::Leave => self.is_member(me) && !self.must_appoint_admin(me),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroupEdit {
    Title(String),
    Description(String),
    /// Disappearing-message timer in seconds (0 = off).
    Timer(u32),
    AddMembers(Vec<Uuid>),
    Remove(Uuid),
    SetAdmin(Uuid, bool),
    Leave,
}

pub fn operations(master_key: &[u8; 32]) -> GroupOperations {
    GroupOperations::new(GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key)))
}

fn aci(id: Uuid) -> Aci {
    Aci::from(id)
}

/// The change actions for `edit` (the version is filled in on sending). For `AddMembers`,
/// `adds` carries a profile key credential per member where we have one; those without
/// are invited instead, as Signal does.
pub fn change_actions(
    ops: &GroupOperations,
    me: Uuid,
    edit: &GroupEdit,
    adds: &[GroupMemberCandidate],
    params: &ServerPublicParams,
) -> Result<Actions, GroupDecodingError> {
    let mut rng = rand::rng();
    let mut actions = Actions::default();
    match edit {
        GroupEdit::Title(t) => {
            actions.modify_title = Some(a::ModifyTitleAction { title: ops.encrypt_title(t.trim(), &mut rng) });
        }
        GroupEdit::Description(d) => {
            let d = Some(d.trim()).filter(|d| !d.is_empty());
            actions.modify_description = Some(a::ModifyDescriptionAction { description: ops.encrypt_description(d, &mut rng) });
        }
        GroupEdit::Timer(secs) => {
            let timer = Timer { duration: *secs };
            actions.modify_disappearing_message_timer = Some(a::ModifyDisappearingMessageTimerAction {
                timer: ops.encrypt_disappearing_messages_timer(Some(&timer), &mut rng),
            });
        }
        GroupEdit::AddMembers(_) => {
            for c in adds {
                match &c.credential {
                    Some(cred) => actions.add_members.push(ops.build_add_member_action_with_credential(cred, Role::Default, params)),
                    None => actions
                        .add_members_pending_profile_key
                        .push(ops.build_add_pending_member_action(c.service_id, aci(me), Role::Default)?),
                }
            }
        }
        GroupEdit::Remove(id) => actions.delete_members.push(ops.build_remove_member_action(aci(*id))?),
        GroupEdit::SetAdmin(id, admin) => {
            // GroupOperations keeps encrypt_aci private; a delete action carries the same ciphertext.
            let user_id = ops.build_remove_member_action(aci(*id))?.deleted_user_id;
            let role = if *admin { Role::Administrator } else { Role::Default };
            actions.modify_member_roles.push(a::ModifyMemberRoleAction { user_id, role: role.into() });
        }
        GroupEdit::Leave => actions.delete_members.push(ops.build_remove_member_action(aci(me))?),
    }
    Ok(actions)
}

/// Signal-Desktop's defaults for a new group: members edit details and add people, no invite link.
pub fn new_group_access() -> presage::libsignal_service::groups_v2::AccessControl {
    presage::libsignal_service::groups_v2::AccessControl {
        attributes: AccessRequired::Member,
        members: AccessRequired::Member,
        add_from_invite_link: AccessRequired::Unsatisfiable,
        member_label: AccessRequired::Member,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::tests::uuid;
    use presage::libsignal_service::configuration::{ServiceConfiguration, SignalServers};
    use presage::libsignal_service::groups_v2::GroupChange;
    use presage::libsignal_service::prelude::ProtobufMessage;
    use presage::libsignal_service::proto::GroupChange as ProtoChange;

    const ME: u8 = 1;
    const ADMIN: u8 = 2;
    const MEMBER: u8 = 3;
    const OUTSIDER: u8 = 9;

    fn info(attributes: AccessRequired, add_members: AccessRequired) -> GroupInfo {
        GroupInfo {
            description: String::new(),
            members: vec![(uuid(ME), false), (uuid(ADMIN), true), (uuid(MEMBER), false)],
            pending: 0,
            requesting: 0,
            attributes,
            add_members,
        }
    }

    #[test]
    fn access_control_decides_who_edits_details_and_adds_members() {
        let g = info(AccessRequired::Member, AccessRequired::Administrator);
        assert!(g.allows_edit(uuid(ME), &GroupEdit::Title("New".into())));
        assert!(!g.allows_edit(uuid(ME), &GroupEdit::Title("  ".into())), "empty title");
        assert!(g.allows_edit(uuid(ME), &GroupEdit::Timer(300)));
        assert!(!g.allows_edit(uuid(ME), &GroupEdit::AddMembers(vec![uuid(OUTSIDER)])), "admins only");
        assert!(g.allows_edit(uuid(ADMIN), &GroupEdit::AddMembers(vec![uuid(OUTSIDER)])));
        assert!(!g.allows_edit(uuid(ADMIN), &GroupEdit::AddMembers(vec![uuid(MEMBER)])), "already in");
        assert!(!g.allows_edit(uuid(OUTSIDER), &GroupEdit::Description("x".into())), "not a member");

        let g = info(AccessRequired::Administrator, AccessRequired::Unsatisfiable);
        assert!(!g.allows_edit(uuid(ME), &GroupEdit::Description("x".into())));
        assert!(g.allows_edit(uuid(ADMIN), &GroupEdit::Description("x".into())));
        assert!(!g.allows_edit(uuid(ADMIN), &GroupEdit::AddMembers(vec![uuid(OUTSIDER)])), "nobody may");
    }

    #[test]
    fn only_admins_remove_and_change_roles_of_others() {
        let g = info(AccessRequired::Member, AccessRequired::Member);
        assert!(!g.allows_edit(uuid(ME), &GroupEdit::Remove(uuid(MEMBER))));
        assert!(g.allows_edit(uuid(ADMIN), &GroupEdit::Remove(uuid(MEMBER))));
        assert!(!g.allows_edit(uuid(ADMIN), &GroupEdit::Remove(uuid(ADMIN))), "leave instead");
        assert!(!g.allows_edit(uuid(ADMIN), &GroupEdit::Remove(uuid(OUTSIDER))));
        assert!(g.allows_edit(uuid(ADMIN), &GroupEdit::SetAdmin(uuid(MEMBER), true)));
        assert!(!g.allows_edit(uuid(ADMIN), &GroupEdit::SetAdmin(uuid(MEMBER), false)), "no-op");
        assert!(!g.allows_edit(uuid(ME), &GroupEdit::SetAdmin(uuid(MEMBER), true)));
    }

    #[test]
    fn last_admin_must_appoint_another_before_leaving() {
        let mut g = info(AccessRequired::Member, AccessRequired::Member);
        assert!(g.allows_edit(uuid(ME), &GroupEdit::Leave));
        assert!(!g.allows_edit(uuid(ADMIN), &GroupEdit::Leave));
        g.members[0].1 = true;
        assert!(g.allows_edit(uuid(ADMIN), &GroupEdit::Leave));
        g.members = vec![(uuid(ADMIN), true)];
        assert!(g.allows_edit(uuid(ADMIN), &GroupEdit::Leave), "alone: nobody to appoint");
        assert!(!g.allows_edit(uuid(OUTSIDER), &GroupEdit::Leave));
    }

    /// Builds the actions, then decrypts them the way receivers do.
    fn roundtrip(edit: GroupEdit, adds: &[GroupMemberCandidate]) -> Vec<GroupChange> {
        let ops = operations(&[7; 32]);
        let params = ServiceConfiguration::from(SignalServers::Production).zkgroup_server_public_params;
        let mut actions = change_actions(&ops, uuid(ME), &edit, adds, &params).expect("build");
        // Set by the server on the signed change.
        actions.source_user_id = ops.build_remove_member_action(aci(uuid(ADMIN))).unwrap().deleted_user_id;
        actions.group_id = vec![0; 32];
        let change = ProtoChange { actions: actions.encode_to_vec(), ..Default::default() };
        ops.decrypt_group_change(change).expect("decrypt").changes
    }

    #[test]
    fn change_actions_decrypt_back_to_the_edit() {
        assert!(matches!(roundtrip(GroupEdit::Title(" Hikers ".into()), &[]).as_slice(), [GroupChange::Title(t)] if t == "Hikers"));
        assert!(matches!(
            roundtrip(GroupEdit::Description("Trails".into()), &[]).as_slice(),
            [GroupChange::Description(Some(d))] if d == "Trails"
        ));
        assert!(matches!(roundtrip(GroupEdit::Description(" ".into()), &[]).as_slice(), [GroupChange::Description(None)]));
        assert!(matches!(
            roundtrip(GroupEdit::Timer(3600), &[]).as_slice(),
            [GroupChange::Timer(Some(Timer { duration: 3600 }))]
        ));
        assert!(matches!(
            roundtrip(GroupEdit::Remove(uuid(MEMBER)), &[]).as_slice(),
            [GroupChange::DeleteMember(a)] if *a == aci(uuid(MEMBER))
        ));
        assert!(matches!(
            roundtrip(GroupEdit::Leave, &[]).as_slice(),
            [GroupChange::DeleteMember(a)] if *a == aci(uuid(ME))
        ));
        assert!(matches!(
            roundtrip(GroupEdit::SetAdmin(uuid(MEMBER), true), &[]).as_slice(),
            [GroupChange::ModifyMemberRole { aci: a, role: Role::Administrator }] if *a == aci(uuid(MEMBER))
        ));
    }

    #[test]
    fn members_without_a_credential_are_invited() {
        let invitee = GroupMemberCandidate { service_id: aci(uuid(OUTSIDER)).into(), credential: None };
        let changes = roundtrip(GroupEdit::AddMembers(vec![uuid(OUTSIDER)]), &[invitee]);
        assert!(
            matches!(changes.as_slice(), [GroupChange::NewPendingMember(p)] if p.address == ServiceId::from(aci(uuid(OUTSIDER)))),
            "{changes:?}"
        );
    }
}
