#[macro_use]
extern crate bitflags;

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use token_ring::{Token, TokenRing};

bitflags! {
    flags NodeType: u8 {
        const NODETYPE_EMPTY     = 0b00001,
        const NODETYPE_HOT       = 0b00010,
        const NODETYPE_COLD      = 0b00100,
        const NODETYPE_TEST      = 0b01000,
        const NODETYPE_MASK      =
        NODETYPE_EMPTY.bits | NODETYPE_HOT.bits | NODETYPE_COLD.bits | NODETYPE_TEST.bits,
        const NODETYPE_REFERENCE = 0b10000
    }
}

struct Node<K, V> {
    key: K,
    value: Option<V>,
    node_type: NodeType,
    phantom_k: PhantomData<K>,
}

pub struct ClockProCache<K, V> {
    capacity: usize,
    test_capacity: usize,
    cold_capacity: usize,
    map: HashMap<K, Token>,
    ring: TokenRing,
    slab: Vec<Node<K, V>>,
    hand_hot: Token,
    hand_cold: Token,
    hand_test: Token,
    count_hot: usize,
    count_cold: usize,
    count_test: usize,
    phantom_k: PhantomData<K>,
}

impl<K, V> ClockProCache<K, V>
    where K: Eq + Hash + Clone
{
    pub fn new(capacity: usize) -> Result<Self, &'static str> {
        Self::new_with_test_capacity(capacity, capacity)
    }

    pub fn new_with_test_capacity(capacity: usize,
                                  test_capacity: usize)
                                  -> Result<Self, &'static str> {
        if capacity < 3 {
            return Err("Cache size cannot be less than 3 entries");
        }
        let mut slab = Vec::with_capacity(capacity + test_capacity);
        unsafe {
            slab.set_len(capacity + test_capacity);
        }
        let cache = ClockProCache {
            capacity: capacity,
            test_capacity: test_capacity,
            cold_capacity: capacity,
            map: HashMap::with_capacity(capacity + test_capacity),
            ring: TokenRing::with_capacity(capacity + test_capacity),
            slab: slab,
            hand_hot: 0,
            hand_cold: 0,
            hand_test: 0,
            count_hot: 0,
            count_cold: 0,
            count_test: 0,
            phantom_k: PhantomData,
        };
        Ok(cache)
    }

    pub fn get_mut<Q: ?Sized>(&mut self, key: &Q) -> Option<&mut V>
        where Q: Hash + Eq,
              K: Borrow<Q>
    {
        let token = match self.map.get(key) {
            None => return None,
            Some(&token) => token,
        };
        let node = &mut self.slab[token];
        if node.value.is_none() {
            return None;
        }
        node.node_type.insert(NODETYPE_REFERENCE);
        Some(node.value.as_mut().unwrap())
    }

    pub fn get<Q: ?Sized>(&mut self, key: &Q) -> Option<&V>
        where Q: Hash + Eq,
              K: Borrow<Q>
    {
        let token = match self.map.get(key) {
            None => return None,
            Some(&token) => token,
        };
        let node = &mut self.slab[token];
        if node.value.is_none() {
            return None;
        }
        node.node_type.insert(NODETYPE_REFERENCE);
        Some(node.value.as_ref().unwrap())
    }

    pub fn contains_key<Q: ?Sized>(&mut self, key: &Q) -> bool
        where Q: Hash + Eq,
              K: Borrow<Q>
    {
        let token = match self.map.get(key) {
            None => return false,
            Some(&token) => token,
        };
        self.slab[token].value.is_some()
    }

    pub fn insert(&mut self, key: K, value: V) -> bool {
        let token = match self.map.get(&key).cloned() {
            None => {
                let node = Node {
                    key: key.clone(),
                    value: Some(value),
                    node_type: NODETYPE_COLD,
                    phantom_k: PhantomData,
                };
                self.meta_add(key, node);
                self.count_cold += 1;
                return true;
            }
            Some(token) => token,
        };
        {
            let mentry = &mut self.slab[token];
            if mentry.value.is_some() {
                mentry.value = Some(value);
                mentry.node_type.insert(NODETYPE_REFERENCE);
                return false;
            }
        }
        if self.cold_capacity < self.capacity {
            self.cold_capacity += 1;
        }
        self.count_test -= 1;
        self.meta_del(token);
        let node = Node {
            key: key.clone(),
            value: Some(value),
            node_type: NODETYPE_HOT,
            phantom_k: PhantomData,
        };
        self.meta_add(key, node);
        self.count_hot += 1;
        false
    }

    fn meta_add(&mut self, key: K, node: Node<K, V>) {
        self.evict();
        let token = self.ring.insert_after(self.hand_hot);
        self.slab[token] = node;
        self.map.insert(key, token);
        if self.hand_cold == self.hand_hot {
            self.hand_cold = self.ring.prev_for_token(self.hand_cold);
        }
    }

    fn evict(&mut self) {
        while self.count_hot + self.count_cold >= self.capacity {
            self.run_hand_cold();
        }
    }

    fn run_hand_cold(&mut self) {
        let mut run_hand_test = false;
        {
            let mentry = &mut self.slab[self.hand_cold];
            if mentry.node_type.intersects(NODETYPE_COLD) {
                if mentry.node_type.intersects(NODETYPE_REFERENCE) {
                    mentry.node_type = NODETYPE_HOT;
                    self.count_cold -= 1;
                    self.count_hot += 1;
                } else {
                    mentry.node_type.remove(NODETYPE_MASK);
                    mentry.node_type.insert(NODETYPE_TEST);
                    mentry.value = None;
                    self.count_cold -= 1;
                    self.count_test += 1;
                    run_hand_test = true
                }
            }
        }
        if run_hand_test {
            while self.count_test > self.test_capacity {
                self.run_hand_test();
            }
        }
        self.hand_cold = self.ring.next_for_token(self.hand_cold);
        while self.count_hot > self.capacity - self.cold_capacity {
            self.run_hand_hot();
        }
    }

    fn run_hand_hot(&mut self) {
        if self.hand_hot == self.hand_test {
            self.run_hand_test();
        }
        {
            let mentry = &mut self.slab[self.hand_hot];
            if mentry.node_type.intersects(NODETYPE_HOT) {
                if mentry.node_type.intersects(NODETYPE_REFERENCE) {
                    mentry.node_type.remove(NODETYPE_REFERENCE);
                } else {
                    mentry.node_type.remove(NODETYPE_MASK);
                    mentry.node_type.insert(NODETYPE_COLD);
                    self.count_hot -= 1;
                    self.count_cold += 1;
                }
            }
        }
        self.hand_hot = self.ring.next_for_token(self.hand_hot);
    }

    fn run_hand_test(&mut self) {
        if self.hand_test == self.hand_cold {
            self.run_hand_cold();
        }
        if self.slab[self.hand_test].node_type.intersects(NODETYPE_TEST) {
            let prev = self.ring.prev_for_token(self.hand_test);
            let hand_test = self.hand_test;
            self.meta_del(hand_test);
            self.hand_test = prev;
            self.count_test -= 1;
            if self.cold_capacity > 1 {
                self.cold_capacity -= 1;
            }
        }
        self.hand_test = self.ring.next_for_token(self.hand_test);
    }

    fn meta_del(&mut self, token: Token) {
        {
            let mentry = &mut self.slab[token];
            mentry.node_type.remove(NODETYPE_MASK);
            mentry.node_type.insert(NODETYPE_EMPTY);
            mentry.value = None;
            self.map.remove(&mentry.key);
        }
        if token == self.hand_hot {
            self.hand_hot = self.ring.prev_for_token(self.hand_hot);
        }
        if token == self.hand_cold {
            self.hand_cold = self.ring.prev_for_token(self.hand_cold);
        }
        if token == self.hand_test {
            self.hand_test = self.ring.prev_for_token(self.hand_test);
        }
        self.ring.remove(token);
    }
}

mod token_ring {
    extern crate slab;

    use self::slab::Slab;

    pub type Token = usize;
    const TOKEN_THUMBSTONE: Token = !0;

    pub struct Node {
        next: Token,
        prev: Token,
    }

    pub struct TokenRing {
        head: Token,
        tail: Token,
        slab: Slab<Node, Token>,
    }

    impl TokenRing {
        pub fn with_capacity(capacity: usize) -> Self {
            if capacity < 1 {
                panic!("A ring cannot have a capacity smaller than 1");
            }
            let slab = Slab::with_capacity(capacity);
            TokenRing {
                head: TOKEN_THUMBSTONE,
                tail: TOKEN_THUMBSTONE,
                slab: slab,
            }
        }

        #[inline]
        pub fn len(&self) -> usize {
            self.slab.len()
        }

        #[inline]
        pub fn next_for_token(&self, token: Token) -> Token {
            let next = self.slab[token].next;
            if next == TOKEN_THUMBSTONE {
                assert!(self.head != TOKEN_THUMBSTONE);
                self.head
            } else {
                next
            }
        }

        #[inline]
        pub fn prev_for_token(&self, token: Token) -> Token {
            let prev = self.slab[token].prev;
            if prev == TOKEN_THUMBSTONE {
                assert!(self.tail != TOKEN_THUMBSTONE);
                self.tail
            } else {
                prev
            }
        }

        pub fn remove(&mut self, token: Token) {
            let (prev, next) = (self.slab[token].prev, self.slab[token].next);
            if prev != TOKEN_THUMBSTONE {
                self.slab[prev].next = next;
            } else {
                self.head = next;
            }
            if next != TOKEN_THUMBSTONE {
                self.slab[next].prev = prev;
            } else {
                self.tail = prev;
            }
            self.slab[token].prev = TOKEN_THUMBSTONE;
            self.slab[token].next = TOKEN_THUMBSTONE;
            self.slab.remove(token);
        }

        pub fn insert_after(&mut self, to: Token) -> Token {
            if self.slab.is_empty() {
                let node = Node {
                    prev: TOKEN_THUMBSTONE,
                    next: TOKEN_THUMBSTONE,
                };
                let token = self.slab.insert(node).ok().expect("Slab full");
                self.head = token;
                self.tail = token;
                return token;
            }
            let to_prev = self.slab[to].prev;
            let old_second = to_prev;
            if old_second == TOKEN_THUMBSTONE {
                let old_second = self.tail;
                let node = Node {
                    prev: old_second,
                    next: TOKEN_THUMBSTONE,
                };
                let token = self.slab.insert(node).ok().expect("Slab full");
                self.slab[old_second].next = token;
                self.tail = token;
                token
            } else {
                let node = Node {
                    prev: old_second,
                    next: to,
                };
                let token = self.slab.insert(node).ok().expect("Slab full");
                self.slab[old_second].next = token;
                self.slab[to].prev = token;
                token
            }
        }
    }
}