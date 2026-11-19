//! Who a write is made for.

use serde::{Deserialize, Serialize};

use crate::ids::UserId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ActorRepr", into = "ActorRepr")]
pub enum Actor {
    User(UserId),
    Agent { on_behalf_of: UserId },
}

/// Wire form of [`Actor`]: `{"user": "<id>"}` / `{"agent": "<id>"}`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActorRepr {
    User(UserId),
    Agent(UserId),
}

impl From<ActorRepr> for Actor {
    fn from(r: ActorRepr) -> Self {
        match r {
            ActorRepr::User(id) => Actor::User(id),
            ActorRepr::Agent(id) => Actor::Agent { on_behalf_of: id },
        }
    }
}

impl From<Actor> for ActorRepr {
    fn from(a: Actor) -> Self {
        match a {
            Actor::User(id) => ActorRepr::User(id),
            Actor::Agent { on_behalf_of } => ActorRepr::Agent(on_behalf_of),
        }
    }
}
