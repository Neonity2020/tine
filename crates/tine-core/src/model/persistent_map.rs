//! The persistent (structurally shared) AVL map behind the graph-text admission
//! index: an exact feed event copies only the search paths it changes, so a
//! snapshot never deep-clones the graph.

use super::*;

pub(super) struct PersistentMapNode<K, V> {
    key: Arc<K>,
    value: Arc<V>,
    left: Option<Arc<PersistentMapNode<K, V>>>,
    right: Option<Arc<PersistentMapNode<K, V>>>,
    height: u8,
    len: usize,
}

/// Small private AVL map whose immutable snapshots share untouched branches.
///
/// Exact feed events copy only the search paths for the exact forward and
/// reverse keys they change. Node keys and values are separately shared so a
/// copied path never deep-clones an owned collision group. This is deliberately
/// not `Arc<BTreeMap<..>>`: copy-on-write over a standard map would still clone
/// the complete graph.
#[derive(Clone)]
pub(super) struct PersistentMap<K, V> {
    pub(super) root: Option<Arc<PersistentMapNode<K, V>>>,
}

impl<K, V> Default for PersistentMap<K, V> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<K: Ord, V> PersistentMap<K, V> {
    fn node(
        key: Arc<K>,
        value: Arc<V>,
        left: Option<Arc<PersistentMapNode<K, V>>>,
        right: Option<Arc<PersistentMapNode<K, V>>>,
    ) -> Arc<PersistentMapNode<K, V>> {
        count_graph_text_admission_persistent_node_allocation();
        Arc::new(PersistentMapNode {
            key,
            value,
            height: 1 + Self::height(&left).max(Self::height(&right)),
            len: 1 + Self::node_len(&left) + Self::node_len(&right),
            left,
            right,
        })
    }

    pub(super) fn height(node: &Option<Arc<PersistentMapNode<K, V>>>) -> u8 {
        node.as_ref().map_or(0, |node| node.height)
    }

    fn node_len(node: &Option<Arc<PersistentMapNode<K, V>>>) -> usize {
        node.as_ref().map_or(0, |node| node.len)
    }

    fn rotate_left(node: &Arc<PersistentMapNode<K, V>>) -> Arc<PersistentMapNode<K, V>> {
        count_graph_text_admission_persistent_rotation();
        let right = node
            .right
            .as_ref()
            .expect("AVL left rotation has a right child");
        let moved = right.left.clone();
        let left = Self::node(
            Arc::clone(&node.key),
            Arc::clone(&node.value),
            node.left.clone(),
            moved,
        );
        Self::node(
            Arc::clone(&right.key),
            Arc::clone(&right.value),
            Some(left),
            right.right.clone(),
        )
    }

    fn rotate_right(node: &Arc<PersistentMapNode<K, V>>) -> Arc<PersistentMapNode<K, V>> {
        count_graph_text_admission_persistent_rotation();
        let left = node
            .left
            .as_ref()
            .expect("AVL right rotation has a left child");
        let moved = left.right.clone();
        let right = Self::node(
            Arc::clone(&node.key),
            Arc::clone(&node.value),
            moved,
            node.right.clone(),
        );
        Self::node(
            Arc::clone(&left.key),
            Arc::clone(&left.value),
            left.left.clone(),
            Some(right),
        )
    }

    fn balanced(
        key: Arc<K>,
        value: Arc<V>,
        mut left: Option<Arc<PersistentMapNode<K, V>>>,
        mut right: Option<Arc<PersistentMapNode<K, V>>>,
    ) -> Arc<PersistentMapNode<K, V>> {
        let balance = i16::from(Self::height(&left)) - i16::from(Self::height(&right));
        if balance > 1 {
            let left_node = left.as_ref().expect("left-heavy AVL node has a left child");
            if Self::height(&left_node.right) > Self::height(&left_node.left) {
                left = Some(Self::rotate_left(left_node));
            }
            return Self::rotate_right(&Self::node(key, value, left, right));
        }
        if balance < -1 {
            let right_node = right
                .as_ref()
                .expect("right-heavy AVL node has a right child");
            if Self::height(&right_node.left) > Self::height(&right_node.right) {
                right = Some(Self::rotate_right(right_node));
            }
            return Self::rotate_left(&Self::node(key, value, left, right));
        }
        Self::node(key, value, left, right)
    }

    fn insert_node(
        node: &Option<Arc<PersistentMapNode<K, V>>>,
        key: Arc<K>,
        value: Arc<V>,
    ) -> (Arc<PersistentMapNode<K, V>>, Option<Arc<V>>) {
        let Some(current) = node else {
            return (Self::node(key, value, None, None), None);
        };
        match key.as_ref().cmp(current.key.as_ref()) {
            std::cmp::Ordering::Less => {
                let (left, previous) = Self::insert_node(&current.left, key, value);
                (
                    Self::balanced(
                        Arc::clone(&current.key),
                        Arc::clone(&current.value),
                        Some(left),
                        current.right.clone(),
                    ),
                    previous,
                )
            }
            std::cmp::Ordering::Greater => {
                let (right, previous) = Self::insert_node(&current.right, key, value);
                (
                    Self::balanced(
                        Arc::clone(&current.key),
                        Arc::clone(&current.value),
                        current.left.clone(),
                        Some(right),
                    ),
                    previous,
                )
            }
            std::cmp::Ordering::Equal => (
                Self::node(key, value, current.left.clone(), current.right.clone()),
                Some(Arc::clone(&current.value)),
            ),
        }
    }

    fn minimum(node: &Arc<PersistentMapNode<K, V>>) -> (&Arc<K>, &Arc<V>) {
        let mut current = node;
        while let Some(left) = &current.left {
            current = left;
        }
        (&current.key, &current.value)
    }

    fn remove_node(
        node: &Option<Arc<PersistentMapNode<K, V>>>,
        key: &K,
    ) -> (Option<Arc<PersistentMapNode<K, V>>>, Option<Arc<V>>) {
        let Some(current) = node else {
            return (None, None);
        };
        match key.cmp(current.key.as_ref()) {
            std::cmp::Ordering::Less => {
                let (left, previous) = Self::remove_node(&current.left, key);
                if previous.is_none() {
                    return (Some(Arc::clone(current)), None);
                }
                (
                    Some(Self::balanced(
                        Arc::clone(&current.key),
                        Arc::clone(&current.value),
                        left,
                        current.right.clone(),
                    )),
                    previous,
                )
            }
            std::cmp::Ordering::Greater => {
                let (right, previous) = Self::remove_node(&current.right, key);
                if previous.is_none() {
                    return (Some(Arc::clone(current)), None);
                }
                (
                    Some(Self::balanced(
                        Arc::clone(&current.key),
                        Arc::clone(&current.value),
                        current.left.clone(),
                        right,
                    )),
                    previous,
                )
            }
            std::cmp::Ordering::Equal => {
                let previous = Some(Arc::clone(&current.value));
                match (&current.left, &current.right) {
                    (None, None) => (None, previous),
                    (Some(left), None) => (Some(Arc::clone(left)), previous),
                    (None, Some(right)) => (Some(Arc::clone(right)), previous),
                    (Some(_), Some(right)) => {
                        let (successor_key, successor_value) = Self::minimum(right);
                        let (new_right, removed) = Self::remove_node(&current.right, successor_key);
                        debug_assert!(removed.is_some());
                        (
                            Some(Self::balanced(
                                Arc::clone(successor_key),
                                Arc::clone(successor_value),
                                current.left.clone(),
                                new_right,
                            )),
                            previous,
                        )
                    }
                }
            }
        }
    }

    pub(super) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut current = self.root.as_deref();
        while let Some(node) = current {
            match key.cmp(node.key.as_ref().borrow()) {
                std::cmp::Ordering::Less => current = node.left.as_deref(),
                std::cmp::Ordering::Greater => current = node.right.as_deref(),
                std::cmp::Ordering::Equal => return Some(node.value.as_ref()),
            }
        }
        None
    }

    pub(super) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.get(key).is_some()
    }

    #[cfg(test)]
    pub(super) fn shared_value<Q>(&self, key: &Q) -> Option<Arc<V>>
    where
        K: std::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut current = self.root.as_deref();
        while let Some(node) = current {
            match key.cmp(node.key.as_ref().borrow()) {
                std::cmp::Ordering::Less => current = node.left.as_deref(),
                std::cmp::Ordering::Greater => current = node.right.as_deref(),
                std::cmp::Ordering::Equal => return Some(Arc::clone(&node.value)),
            }
        }
        None
    }

    pub(super) fn insert(&mut self, key: K, value: V) -> Option<Arc<V>> {
        let (root, previous) = Self::insert_node(&self.root, Arc::new(key), Arc::new(value));
        self.root = Some(root);
        previous
    }

    pub(super) fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        let (root, previous) = Self::remove_node(&self.root, key);
        self.root = root;
        previous
    }

    pub(super) fn iter(&self) -> PersistentMapIter<'_, K, V> {
        PersistentMapIter::new(self.root.as_deref())
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(key, _)| key)
    }

    pub(super) fn path_copy_peak_upper_bound(&self) -> io::Result<u64> {
        // Deletion can copy both the search path and the in-order-successor
        // path. Each level can allocate five nodes for a double rotation.
        let copied_nodes = checked_add_bytes(
            checked_mul_bytes(u64::from(Self::height(&self.root)), 10)?,
            1,
        )?;
        checked_mul_bytes(
            copied_nodes,
            usize_to_u64(std::mem::size_of::<PersistentMapNode<K, V>>())?,
        )
    }
}

impl<K: Ord, V> FromIterator<(K, V)> for PersistentMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut map = Self::default();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

pub(super) struct PersistentMapIter<'a, K, V> {
    stack: Vec<&'a PersistentMapNode<K, V>>,
}

impl<'a, K, V> PersistentMapIter<'a, K, V> {
    fn new(root: Option<&'a PersistentMapNode<K, V>>) -> Self {
        let mut iter = Self { stack: Vec::new() };
        iter.push_left(root);
        iter
    }

    fn push_left(&mut self, mut node: Option<&'a PersistentMapNode<K, V>>) {
        while let Some(current) = node {
            self.stack.push(current);
            node = current.left.as_deref();
        }
    }
}

impl<'a, K, V> Iterator for PersistentMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        self.push_left(node.right.as_deref());
        Some((node.key.as_ref(), node.value.as_ref()))
    }
}

impl<'a, K: Ord, V> IntoIterator for &'a PersistentMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = PersistentMapIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
