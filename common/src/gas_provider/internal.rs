// This file is part of Gear.

// Copyright (C) 2021-2022 Gear Technologies Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use super::*;
use crate::storage::MapStorage;

/// Output of `TreeImpl::catch_value` call.
#[derive(Debug, Clone, Copy)]
enum CatchValueOutput<Balance> {
    /// Catching value is impossible, therefore blocked.
    Blocked,
    /// Value was not caught, because was moved to the patron node.
    ///
    /// For more info about patron nodes see `TreeImpl::find_ancestor_patron`
    Missed,
    /// Value was caught and will be removed from the node
    Caught(Balance),
}

impl<Balance: BalanceTrait> CatchValueOutput<Balance> {
    fn into_consume_output<TotalValue, ExternalId>(
        self,
        origin: ExternalId,
    ) -> Option<(NegativeImbalance<Balance, TotalValue>, ExternalId)>
    where
        TotalValue: ValueStorage<Value = Balance>,
        ExternalId: Clone,
    {
        match self {
            CatchValueOutput::Caught(value) => Some((NegativeImbalance::new(value), origin)),
            _ => None,
        }
    }

    fn is_blocked(&self) -> bool {
        matches!(self, CatchValueOutput::Blocked)
    }

    fn is_caught(&self) -> bool {
        matches!(self, CatchValueOutput::Caught(_))
    }
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum NodeCreationKey<MapKey, MapReservationKey> {
    Cut(MapKey),
    SpecifiedLocal(MapKey),
    Reserved(MapReservationKey),
}

impl<MapKey, MapReservationKey> NodeCreationKey<MapKey, MapReservationKey>
where
    MapKey: Copy,
    MapReservationKey: Copy,
{
    fn to_gas_node_id(self) -> GasNodeId<MapKey, MapReservationKey> {
        match self {
            NodeCreationKey::Cut(key) => GasNodeId::Node(key),
            NodeCreationKey::SpecifiedLocal(key) => GasNodeId::Node(key),
            NodeCreationKey::Reserved(key) => GasNodeId::Reservation(key),
        }
    }
}

pub struct TreeImpl<TotalValue, InternalError, Error, ExternalId, StorageMap>(
    PhantomData<(TotalValue, InternalError, Error, ExternalId, StorageMap)>,
);

impl<
        TotalValue,
        Balance,
        InternalError,
        Error,
        MapKey,
        MapReservationKey,
        ExternalId,
        StorageMap,
    > TreeImpl<TotalValue, InternalError, Error, ExternalId, StorageMap>
where
    Balance: BalanceTrait,
    TotalValue: ValueStorage<Value = Balance>,
    InternalError: super::Error,
    Error: From<InternalError>,
    ExternalId: Clone,
    MapKey: Copy,
    MapReservationKey: Copy,
    StorageMap: MapStorage<
        Key = GasNodeId<MapKey, MapReservationKey>,
        Value = GasNode<ExternalId, MapKey, Balance>,
    >,
    GasNodeId<MapKey, MapReservationKey>: From<MapKey> + From<MapReservationKey>,
{
    pub(super) fn get_node(
        key: impl Into<GasNodeId<MapKey, MapReservationKey>>,
    ) -> Option<StorageMap::Value> {
        StorageMap::get(&key.into())
    }

    /// Returns the first parent, that is able to hold a concrete value, but
    /// doesn't necessarily have a non-zero value, along with it's id.
    ///
    /// Node itself is considered as a self-parent too. The gas tree holds
    /// invariant, that all the nodes with unspecified value always have a
    /// parent with a specified value.
    ///
    /// The id of the returned node is of `Option` type. If it's `None`, it
    /// means, that the ancestor and `self` are the same.
    pub(super) fn node_with_value(
        node: StorageMap::Value,
    ) -> Result<(StorageMap::Value, Option<MapKey>), Error> {
        let mut ret_node = node;
        let mut ret_id = None;
        if let GasNode::UnspecifiedLocal { parent, .. } = ret_node {
            ret_id = Some(parent);
            ret_node = Self::get_node(parent).ok_or_else(InternalError::parent_is_lost)?;
            if !(ret_node.is_external() || ret_node.is_specified_local()) {
                return Err(InternalError::unexpected_node_type().into());
            }
        }

        Ok((ret_node, ret_id))
    }

    /// Returns id and data for root node (as [`GasNode`]) of the value tree,
    /// which contains the `node`. If some node along the upstream path is
    /// missing, returns an error (tree is invalidated).
    ///
    /// As in [`TreeImpl::node_with_value`], root's id is of `Option` type. It
    /// is equal to `None` in case `node` is a root node.
    pub(super) fn root(
        node: StorageMap::Value,
    ) -> Result<(StorageMap::Value, Option<MapKey>), Error> {
        let mut ret_id = None;
        let mut ret_node = node;
        while let Some(parent) = ret_node.parent() {
            ret_id = Some(parent);
            ret_node = Self::get_node(parent).ok_or_else(InternalError::parent_is_lost)?;
        }

        Ok((ret_node, ret_id))
    }

    pub(super) fn decrease_parents_ref(node: &StorageMap::Value) -> Result<(), Error> {
        let id = match node.parent() {
            Some(id) => id,
            None => return Ok(()),
        };

        let mut parent = Self::get_node(id).ok_or_else(InternalError::parent_is_lost)?;
        if parent.refs() == 0 {
            return Err(InternalError::parent_has_no_children().into());
        }

        match node {
            GasNode::SpecifiedLocal { .. } => {
                parent.decrease_spec_refs();
            }
            GasNode::UnspecifiedLocal { .. } => {
                parent.decrease_unspec_refs();
            }
            _ => return Err(InternalError::unexpected_node_type().into()),
        }

        // Update parent node
        StorageMap::insert(id.into(), parent);

        Ok(())
    }

    /// Tries to __"catch"__ the value inside the node if possible.
    ///
    /// If the node is a patron or of unspecified type, value is blocked, i.e.
    /// can't be removed or impossible to hold value to be removed.
    ///
    /// If the node is not a patron, but it has an ancestor patron, value is
    /// moved to it. So the patron's balance is increased (mutated).
    /// Otherwise the value is caught and removed from the tree. In both
    /// cases the `self` node's balance is zeroed.
    ///
    /// # Note
    /// Method doesn't mutate `self` in the storage, but only changes it's
    /// balance in memory.
    fn catch_value(node: &mut StorageMap::Value) -> Result<CatchValueOutput<Balance>, Error> {
        if node.is_patron() {
            return Ok(CatchValueOutput::Blocked);
        }

        if !node.is_unspecified_local() {
            if let Some((mut patron, patron_id)) = Self::find_ancestor_patron(node)? {
                let self_value = node
                    .value_mut()
                    .ok_or_else(InternalError::unexpected_node_type)?;
                if self_value.is_zero() {
                    // Early return to prevent redundant storage look-ups
                    return Ok(CatchValueOutput::Missed);
                }
                let patron_value = patron
                    .value_mut()
                    .ok_or_else(InternalError::unexpected_node_type)?;
                *patron_value = patron_value.saturating_add(*self_value);
                *self_value = Zero::zero();
                StorageMap::insert(patron_id.into(), patron);

                Ok(CatchValueOutput::Missed)
            } else {
                let self_value = node
                    .value_mut()
                    .ok_or_else(InternalError::unexpected_node_type)?;
                let value_copy = *self_value;
                *self_value = Zero::zero();

                Ok(CatchValueOutput::Caught(value_copy))
            }
        } else {
            Ok(CatchValueOutput::Blocked)
        }
    }

    /// Looks for `self` node's patron ancestor.
    ///
    /// A patron node is the node, on which some other nodes in the tree rely.
    /// More precisely, unspecified local nodes rely on nodes with value, so
    /// specified nodes as `GasNode::External` and `GasNode::SpecifiedLocal`
    /// are patron ones. The other criteria for a node to be marked as the
    /// patron one is not being consumed - value of such nodes mustn't be
    /// moved, because node itself rely on it.
    fn find_ancestor_patron(
        node: &StorageMap::Value,
    ) -> Result<Option<(StorageMap::Value, MapKey)>, Error> {
        match node {
            GasNode::External { .. } | GasNode::Cut { .. } | GasNode::Reserved { .. } => Ok(None),
            GasNode::SpecifiedLocal { parent, .. } => {
                let mut ret_id = *parent;
                let mut ret_node =
                    Self::get_node(*parent).ok_or_else(InternalError::parent_is_lost)?;
                while !ret_node.is_patron() {
                    match ret_node {
                        GasNode::External { .. } => return Ok(None),
                        GasNode::SpecifiedLocal { parent, .. } => {
                            ret_id = parent;
                            ret_node =
                                Self::get_node(parent).ok_or_else(InternalError::parent_is_lost)?;
                        }
                        _ => return Err(InternalError::unexpected_node_type().into()),
                    }
                }
                Ok(Some((ret_node, ret_id)))
            }
            // Although unspecified local type has a patron parent, it's considered
            // an error to call the method from that type of gas node.
            GasNode::UnspecifiedLocal { .. } => Err(InternalError::forbidden().into()),
        }
    }

    /// Tries to remove consumed nodes on the same path from the `key` node to
    /// the root (including it). While trying to remove nodes, also catches
    /// value stored in them is performed.
    ///
    /// Value catch is performed for all the non-patron nodes on the path from
    /// `key` to root, until some patron node is reached. By the invariant,
    /// catching can't be blocked, because the node is not a patron.
    ///
    /// For node removal there are 2 main requirements:
    /// 1. It's not a patron node
    /// 2. It doesn't have any children nodes.
    ///
    /// Although the value in nodes is moved or returned to the origin, calling
    /// `GasNode::catch_value` in this procedure can still result in catching
    /// non-zero value. That's possible for example, when gasful parent is
    /// consumed and has a gas-less child. When gas-less child is consumed
    /// in `ValueTree::consume` call, the gasful parent's value is caught
    /// in this function.
    ///
    /// # Invariants
    /// Internal invariant of the procedure:
    ///
    /// 1. If `catch_value` call ended up with `CatchValueOutput::Missed` in
    /// `consume`, all the calls of catch_value on ancestor nodes will be
    /// `CatchValueOutput::Missed` as well.
    /// That's because if there is an existing ancestor patron on the path from
    /// the `key` node to the root, catching value on all the nodes before that
    /// patron on this same path will give the same `CatchValueOutput::Missed`
    /// result due to the fact that they all have same ancestor patron, which
    /// will receive their values.
    ///
    /// 2. Also in that case cascade ancestors consumption will last until
    /// either the patron node or the first ancestor with specified child found.
    ///
    /// 3. If `catch_value` call ended up with `CatchValueOutput::Caught(x)` in
    /// `consume`, all the calls of `catch_value` on ancestor nodes will be
    /// `CatchValueOutput::Caught(0)`.
    /// That's due to the 12-th invariant stated in [`super::property_tests`]
    /// module docs. When node becomes consumed without unspec refs (i.e.,
    /// stops being a patron) `consume` procedure call on such node either
    /// moves value upstream (if there is an ancestor patron) or returns
    /// value to the origin. So any repetitive `catch_value` call on such
    /// nodes results in `CatchValueOutput::Caught(0)` (if there is an
    /// ancestor patron).
    /// So if `consume` procedure on the node with `key` id resulted in value
    /// being caught, it means that there are no ancestor patrons, so none of
    /// `catch_value` calls on the node's ancestors will return
    /// `CatchValueOutput::Missed`, but will return
    /// `CatchValueOutput::Caught(0)`.
    fn try_remove_consumed_ancestors(
        key: MapKey,
        descendant_catch_output: CatchValueOutput<Balance>,
    ) -> ConsumeResultOf<Self> {
        let mut node_id = key;

        let mut node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;
        let mut consume_output = None;
        let external = Self::get_external(key)?;

        // Descendant's `catch_value` output is used for the sake of optimization.
        // We could easily run `catch_value` in the below `while` loop each time
        // we process the ancestor. But that would lead to quadratic complexity
        // of the `consume` & `try_remove_consumed_ancestors` procedures.
        //
        // In order to optimize that we use internal properties of the `consume`
        // procedure described in the function's docs. The general idea of the
        // optimization is that in some situations there is no need in
        // `catch_value` call, because results will be the same for
        // all the ancestors.
        let mut catch_output = if descendant_catch_output.is_caught() {
            CatchValueOutput::Caught(Zero::zero())
        } else {
            descendant_catch_output
        };
        while !node.is_patron() {
            if catch_output.is_blocked() {
                catch_output = Self::catch_value(&mut node)?;
            }

            // The node is not a patron and can't be of unspecified type.
            if catch_output.is_blocked() {
                return Err(InternalError::value_is_blocked().into());
            }

            consume_output =
                consume_output.or_else(|| catch_output.into_consume_output(external.clone()));

            if node.spec_refs() == 0 {
                Self::decrease_parents_ref(&node)?;
                StorageMap::remove(node_id.into());

                match node {
                    GasNode::External { .. } => {
                        if !catch_output.is_caught() {
                            return Err(InternalError::value_is_not_caught().into());
                        }
                        return Ok(consume_output);
                    }
                    GasNode::SpecifiedLocal { parent, .. } => {
                        let parent = parent;
                        node_id = parent;
                        node = Self::get_node(parent).ok_or_else(InternalError::parent_is_lost)?;
                    }
                    _ => return Err(InternalError::unexpected_node_type().into()),
                }
            } else {
                StorageMap::insert(node_id.into(), node);
                return Ok(consume_output);
            }
        }

        Ok(consume_output)
    }

    /// Create ValueNode from node key with value
    ///
    /// if `reserve`, create ValueType::ReservedLocal
    /// else, create ValueType::SpecifiedLocal
    pub(super) fn create_from_with_value(
        key: MapKey,
        new_node_key: NodeCreationKey<MapKey, MapReservationKey>,
        amount: Balance,
    ) -> Result<(), Error> {
        let (mut node, node_id) =
            Self::node_with_value(Self::get_node(key).ok_or_else(InternalError::node_not_found)?)?;
        let node_id = node_id.unwrap_or(key);

        // Check if the parent node is detached
        if node.is_detached() {
            return Err(InternalError::forbidden().into());
        }

        // This also checks if key == new_node_key
        if StorageMap::contains_key(&new_node_key.to_gas_node_id()) {
            return Err(InternalError::node_already_exists().into());
        }

        // A `node` is guaranteed to have inner_value here, because
        // it was queried after `Self::node_with_value` call.
        if node
            .value()
            .ok_or_else(InternalError::unexpected_node_type)?
            < amount
        {
            return Err(InternalError::insufficient_balance().into());
        }

        // Detect inner from `reserve`.
        let new_node = match new_node_key {
            NodeCreationKey::Cut(_) => {
                let id = Self::get_external(key)?;
                GasNode::Cut { id, value: amount }
            }
            NodeCreationKey::SpecifiedLocal(_) => {
                node.increase_spec_refs();

                GasNode::SpecifiedLocal {
                    value: amount,
                    lock: Zero::zero(),
                    parent: node_id,
                    refs: Default::default(),
                    consumed: false,
                }
            }
            NodeCreationKey::Reserved(_) => {
                let id = Self::get_external(key)?;
                GasNode::Reserved {
                    id,
                    value: amount,
                    lock: Zero::zero(),
                }
            }
        };

        // Save new node
        StorageMap::insert(new_node_key.to_gas_node_id(), new_node);

        let node_value = node
            .value_mut()
            .ok_or_else(InternalError::unexpected_node_type)?;

        *node_value = node_value.saturating_sub(amount);

        StorageMap::insert(node_id.into(), node);

        Ok(())
    }
}

impl<
        TotalValue,
        Balance,
        InternalError,
        Error,
        MapKey,
        MapReservationKey,
        ExternalId,
        StorageMap,
    > Tree for TreeImpl<TotalValue, InternalError, Error, ExternalId, StorageMap>
where
    Balance: BalanceTrait,
    TotalValue: ValueStorage<Value = Balance>,
    InternalError: super::Error,
    Error: From<InternalError>,
    ExternalId: Clone,
    MapKey: Copy,
    MapReservationKey: Copy,
    StorageMap: MapStorage<
        Key = GasNodeId<MapKey, MapReservationKey>,
        Value = GasNode<ExternalId, MapKey, Balance>,
    >,
    GasNodeId<MapKey, MapReservationKey>: From<MapKey> + From<MapReservationKey>,
{
    type ExternalOrigin = ExternalId;
    type Key = MapKey;
    type ReservationKey = MapReservationKey;
    type Balance = Balance;

    type PositiveImbalance = PositiveImbalance<Balance, TotalValue>;
    type NegativeImbalance = NegativeImbalance<Balance, TotalValue>;

    type InternalError = InternalError;
    type Error = Error;

    fn total_supply() -> Self::Balance {
        TotalValue::get().unwrap_or_else(Zero::zero)
    }

    fn create(
        origin: Self::ExternalOrigin,
        key: Self::Key,
        amount: Self::Balance,
    ) -> Result<Self::PositiveImbalance, Self::Error> {
        let key = key.into();

        if StorageMap::contains_key(&key) {
            return Err(InternalError::node_already_exists().into());
        }

        let node = GasNode::new(origin, amount);

        // Save value node to storage
        StorageMap::insert(key, node);

        Ok(PositiveImbalance::new(amount))
    }

    fn get_origin_node(
        key: impl Into<GasNodeIdOf<Self>>,
    ) -> Result<(Self::ExternalOrigin, GasNodeIdOf<Self>), Self::Error> {
        let key = key.into();
        let node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;

        // key known, must return the origin, unless corrupted
        let (root, maybe_key) = Self::root(node)?;

        if let GasNode::External { id, .. }
        | GasNode::Cut { id, .. }
        | GasNode::Reserved { id, .. } = root
        {
            Ok((id, maybe_key.map(GasNodeId::Node).unwrap_or(key)))
        } else {
            unreachable!("Guaranteed by ValueNode::root method")
        }
    }

    fn get_limit_node(key: Self::Key) -> Result<(Self::Balance, Self::Key), Self::Error> {
        let node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;

        let (node_with_value, maybe_key) = Self::node_with_value(node)?;

        // The node here is either external or specified, hence has the inner value
        let v = node_with_value
            .value()
            .ok_or_else(InternalError::unexpected_node_type)?;

        Ok((v, maybe_key.unwrap_or(key)))
    }

    /// Marks a node with `key` as consumed, if possible, and tries to return
    /// it's value and delete it. The function performs same procedure with all
    /// the nodes on the path from it to the root, if possible.
    ///
    /// Marking a node as `consumed` is possible only for `GasNode::External`
    /// and `GasNode::SpecifiedLocal` nodes. That is because these nodes can
    /// be not deleted after the function call, because of, for instance,
    /// having children refs. Such nodes as `GasNode::UnspecifiedLocal`
    /// and `GasNode::ReservedLocal` are removed when the function is
    /// called, so there is no need for marking them as consumed.
    ///
    /// When consuming the node, it's value is mutated by calling `catch_value`,
    /// which tries to either return or move value upstream if possible.
    /// Read the `catch_value` function's documentation for details.
    ///
    /// To delete node, here should be two requirements:
    /// 1. `Self::consume` was called on the node.
    /// 2. The node has no children, i.e. spec/unspec refs.
    ///
    /// So if it's impossible to delete a node, then it's impossible to delete
    /// its parent in the current call. Also if it's possible to delete a node,
    /// then it doesn't necessarily mean that its parent will be deleted. An
    /// example here could be the case, when during async execution original
    /// message went to wait list, so wasn't consumed but the one generated
    /// during the execution of the original message went to message queue
    /// and was successfully executed.
    fn consume(key: impl Into<GasNodeIdOf<Self>>) -> ConsumeResultOf<Self> {
        let key = key.into();
        let mut node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;

        if node.is_consumed() {
            return Err(InternalError::node_was_consumed().into());
        }

        if let Some(lock) = node.lock() {
            if !lock.is_zero() {
                return Err(InternalError::consumed_with_lock().into());
            }
        }

        node.mark_consumed();
        let catch_output = Self::catch_value(&mut node)?;
        let external = Self::get_external(key)?;

        let res = if node.refs() == 0 {
            Self::decrease_parents_ref(&node)?;
            StorageMap::remove(key);

            match node {
                GasNode::External { .. } | GasNode::Cut { .. } | GasNode::Reserved { .. } => {
                    if !catch_output.is_caught() {
                        return Err(InternalError::value_is_not_caught().into());
                    }
                    catch_output.into_consume_output(external)
                }
                GasNode::UnspecifiedLocal { parent, .. } => {
                    if !catch_output.is_blocked() {
                        return Err(InternalError::value_is_not_blocked().into());
                    }
                    Self::try_remove_consumed_ancestors(parent, catch_output)?
                }
                GasNode::SpecifiedLocal { parent, .. } => {
                    if catch_output.is_blocked() {
                        return Err(InternalError::value_is_blocked().into());
                    }
                    let consume_output = catch_output.into_consume_output(external);
                    let consume_ancestors_output =
                        Self::try_remove_consumed_ancestors(parent, catch_output)?;
                    match (&consume_output, consume_ancestors_output) {
                        // value can't be caught in both procedures
                        (Some(_), Some((neg_imb, _))) if neg_imb.peek().is_zero() => consume_output,
                        (None, None) => consume_output,
                        _ => return Err(InternalError::unexpected_consume_output().into()),
                    }
                }
            }
        } else {
            if node.is_cut() || node.is_unspecified_local() {
                return Err(InternalError::unexpected_node_type().into());
            }

            StorageMap::insert(key, node);
            catch_output.into_consume_output(external)
        };

        Ok(res)
    }

    /// Spends `amount` of gas from the ancestor of node with `key` id.
    ///
    /// Calling the function is possible even if an ancestor is consumed.
    ///
    /// ### Note:
    /// Node is considered as an ancestor of itself.
    fn spend(
        key: Self::Key,
        amount: Self::Balance,
    ) -> Result<Self::NegativeImbalance, Self::Error> {
        // Upstream node with a concrete value exist for any node.
        // If it doesn't, the tree is considered invalidated.
        let (mut node, node_id) =
            Self::node_with_value(Self::get_node(key).ok_or_else(InternalError::node_not_found)?)?;

        // A `node` is guaranteed to have inner_value here, because it was
        // queried after `Self::node_with_value` call.
        let node_value = node
            .value_mut()
            .ok_or_else(InternalError::unexpected_node_type)?;

        if *node_value < amount {
            return Err(InternalError::insufficient_balance().into());
        }

        *node_value = node_value.saturating_sub(amount);
        log::debug!("Spent {:?} of gas", amount);

        // Save node that delivers limit
        StorageMap::insert(node_id.unwrap_or(key).into(), node);

        Ok(NegativeImbalance::new(amount))
    }

    fn split_with_value(
        key: Self::Key,
        new_key: Self::Key,
        amount: Self::Balance,
    ) -> Result<(), Self::Error> {
        Self::create_from_with_value(key, NodeCreationKey::SpecifiedLocal(new_key), amount)
    }

    fn split(key: Self::Key, new_key: Self::Key) -> Result<(), Self::Error> {
        let new_key = new_key.into();

        let (mut node, node_id) =
            Self::node_with_value(Self::get_node(key).ok_or_else(InternalError::node_not_found)?)?;
        let node_id = node_id.unwrap_or(key);

        // Check if the value node is detached
        if node.is_detached() {
            return Err(InternalError::forbidden().into());
        }

        // This also checks if key == new_node_key
        if StorageMap::contains_key(&new_key) {
            return Err(InternalError::node_already_exists().into());
        }

        node.increase_unspec_refs();

        let new_node = GasNode::UnspecifiedLocal {
            parent: node_id,
            lock: Zero::zero(),
        };

        // Save new node
        StorageMap::insert(new_key, new_node);
        // Update current node
        StorageMap::insert(node_id.into(), node);

        Ok(())
    }

    fn cut(key: Self::Key, new_key: Self::Key, amount: Self::Balance) -> Result<(), Self::Error> {
        Self::create_from_with_value(key, NodeCreationKey::Cut(new_key), amount)
    }

    fn lock(key: impl Into<GasNodeIdOf<Self>>, amount: Self::Balance) -> Result<(), Self::Error> {
        let key = key.into();

        // Taking node to lock into.
        let node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;

        // Validating node type to be able to lock.
        if !node.is_lockable() {
            return Err(InternalError::forbidden().into());
        }

        // Validating that node is not consumed.
        if node.is_consumed() {
            return Err(InternalError::node_was_consumed().into());
        }

        // Quick quit on queried zero lock.
        if amount.is_zero() {
            return Ok(());
        }

        // Taking value provider for this node.
        let (mut ancestor_node, ancestor_id) = Self::node_with_value(node)?;

        // Mutating value of provider.
        let ancestor_node_value = ancestor_node
            .value_mut()
            .ok_or_else(InternalError::unexpected_node_type)?;

        if *ancestor_node_value < amount {
            return Err(InternalError::insufficient_balance().into());
        }

        *ancestor_node_value = ancestor_node_value.saturating_sub(amount);

        // If provider is a parent, we save it to storage, otherwise mutating
        // current node further, saving it afterward.
        let mut node = if let Some(ancestor_id) = ancestor_id {
            StorageMap::insert(ancestor_id.into(), ancestor_node);

            // Unreachable error: the same queried at the beginning of function.
            Self::get_node(key).ok_or_else(InternalError::node_not_found)?
        } else {
            ancestor_node
        };

        let node_lock = node
            .lock_mut()
            .ok_or_else(InternalError::unexpected_node_type)?;

        *node_lock = node_lock.saturating_add(amount);

        StorageMap::insert(key, node);

        Ok(())
    }

    // Such implementation of moving value upper works, because:
    //
    // - For value-holding types (`GasNode::External` and
    // `GasNode::SpecifiedLocal`) locking and unlocking on consumed node is denied at the moment,
    // so on lock and unlock they will update only themselves.
    //
    // - For non-value-holding type (`GasNode::UnspecifiedLocal`) locking and
    // unlocking chained with value-holding parent, which cannot be freed
    // (can't move its balance upstream), due to existence of this
    // unspecified node, referring it.
    //
    // - For reservation type (`GasNode::ReservedLocal`) locking is denied.
    fn unlock(key: impl Into<GasNodeIdOf<Self>>, amount: Self::Balance) -> Result<(), Self::Error> {
        let key = key.into();

        // Taking node to unlock from.
        let mut node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;

        // Validating node type to be able to lock.
        if !node.is_lockable() {
            return Err(InternalError::forbidden().into());
        }

        // Validating that node is not consumed.
        if node.is_consumed() {
            return Err(InternalError::node_was_consumed().into());
        }

        // Quick quit on queried zero unlock.
        if amount.is_zero() {
            return Ok(());
        }

        // Mutating locked value of queried node.
        let node_lock = node
            .lock_mut()
            .ok_or_else(InternalError::unexpected_node_type)?;

        if *node_lock < amount {
            return Err(InternalError::insufficient_balance().into());
        }

        *node_lock = node_lock.saturating_sub(amount);

        // Taking value provider for this node.
        let (ancestor_node, ancestor_id) = Self::node_with_value(node.clone())?;

        // Mutating value of provider.
        // If provider is a current node, we save it to storage, otherwise mutating
        // provider node further, saving it afterward.
        let (mut ancestor_node, ancestor_id) = if let Some(ancestor_id) = ancestor_id {
            StorageMap::insert(key, node);

            (ancestor_node, ancestor_id.into())
        } else {
            (node, key)
        };

        let ancestor_value = ancestor_node
            .value_mut()
            .ok_or_else(InternalError::unexpected_node_type)?;

        *ancestor_value = ancestor_value.saturating_add(amount);

        StorageMap::insert(ancestor_id, ancestor_node);

        Ok(())
    }

    fn get_lock(key: impl Into<GasNodeIdOf<Self>>) -> Result<Self::Balance, Self::Error> {
        let key = key.into();
        let node = Self::get_node(key).ok_or_else(InternalError::node_not_found)?;

        node.lock().ok_or_else(|| InternalError::forbidden().into())
    }

    fn reserve(
        key: Self::Key,
        new_key: Self::ReservationKey,
        amount: Self::Balance,
    ) -> Result<(), Self::Error> {
        Self::create_from_with_value(key, NodeCreationKey::Reserved(new_key), amount)
    }

    fn exists(key: Self::Key) -> bool {
        Self::get_node(key).is_some()
    }

    fn clear() {
        TotalValue::kill();
        StorageMap::clear();
    }
}
