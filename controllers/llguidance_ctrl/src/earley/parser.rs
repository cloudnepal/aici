use std::{
    collections::HashMap,
    fmt::{Debug, Display},
    hash::Hash,
    ops::Range,
    sync::Arc,
    vec,
};

use aici_abi::{
    svob::SimpleVob,
    toktree::{Recognizer, SpecialToken, TokTrie},
    TokenId,
};
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::{api::GenGrammarOptions, earley::lexer::Lexer};

use super::{
    grammar::{CGrammar, CSymIdx, CSymbol, ModelVariable, RuleIdx},
    lexer::{LexerResult, PreLexeme, StateID},
    lexerspec::{Lexeme, LexemeIdx, LexerSpec},
};

const TRACE: bool = false;
const DEBUG: bool = true;

const MAX_ROW: usize = 100;

macro_rules! trace {
    ($($arg:tt)*) => {
        if cfg!(feature = "logging") && TRACE {
            eprintln!($($arg)*);
        }
    }
}

macro_rules! debug {
    ($($arg:tt)*) => {
        if cfg!(feature = "logging") && DEBUG {
            eprintln!($($arg)*);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Item {
    data: u64,
}

// These are only tracked in definitive mode
#[derive(Debug, Clone)]
struct ItemProps {
    // TODO remove; we're no longer using this
    hidden_start: usize,
}

impl Display for ItemProps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.hidden_start == usize::MAX {
            write!(f, "")
        } else {
            write!(f, "(hidden_start {})", self.hidden_start)
        }
    }
}

impl Default for ItemProps {
    fn default() -> Self {
        ItemProps {
            hidden_start: usize::MAX,
        }
    }
}

impl ItemProps {
    fn merge(&mut self, other: ItemProps) {
        self.hidden_start = self.hidden_start.min(other.hidden_start);
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct ParserStats {
    pub rows: usize,
    pub definitive_bytes: usize,
    pub lexer_ops: usize,
    pub all_items: usize,
    pub hidden_bytes: usize,
}

impl ParserStats {
    pub fn delta(&self, previous: &ParserStats) -> ParserStats {
        ParserStats {
            rows: self.rows - previous.rows,
            definitive_bytes: self.definitive_bytes - previous.definitive_bytes,
            lexer_ops: self.lexer_ops - previous.lexer_ops,
            all_items: self.all_items - previous.all_items,
            hidden_bytes: self.hidden_bytes - previous.hidden_bytes,
        }
    }
}

#[derive(Clone)]
struct Row {
    first_item: usize,
    last_item: usize,
    allowed_lexemes: SimpleVob,
}

impl Row {
    fn item_indices(&self) -> Range<usize> {
        self.first_item..self.last_item
    }
}

impl Item {
    #[allow(dead_code)]
    const NULL: Self = Item { data: 0 };

    fn new(rule: RuleIdx, start: usize) -> Self {
        Item {
            data: rule.as_index() as u64 | ((start as u64) << 32),
        }
    }

    fn rule_idx(&self) -> RuleIdx {
        RuleIdx::from_index(self.data as u32)
    }

    fn start_pos(&self) -> usize {
        (self.data >> 32) as usize
    }

    fn advance_dot(&self) -> Self {
        Item {
            data: self.data + 1,
        }
    }
}

#[derive(Clone)]
struct Scratch {
    grammar: Arc<CGrammar>,
    row_start: usize,
    row_end: usize,
    items: Vec<Item>,
    item_props: Vec<ItemProps>,
    definitive: bool,
}

#[derive(Clone)]
struct RowInfo {
    start_byte_idx: usize,
    lexeme: Lexeme,
    token_idx_start: usize,
    token_idx_stop: usize,
    max_tokens: HashMap<LexemeIdx, usize>,
}

impl RowInfo {
    fn apply_token_idx(&mut self, idx: usize) {
        self.token_idx_start = self.token_idx_start.min(idx);
        self.token_idx_stop = self.token_idx_stop.max(idx);
    }

    fn dbg(&self, lexspec: &LexerSpec) -> String {
        format!(
            "token_idx: {}-{} {} {}",
            self.token_idx_start,
            self.token_idx_stop,
            lexspec.dbg_lexeme(&self.lexeme),
            if self.max_tokens.is_empty() {
                "".to_string()
            } else {
                format!("max_tokens={:?}", self.max_tokens)
            }
        )
    }
}

// State transition is:
// if (next_lexeme, next_lexer_state) := lexer(top.lexer_state, next_byte) {
//     row_idx = scan(top.row_idx, next_lexeme)
//     push(LexerState { row_idx, next_byte, next_lexer_state })
// }
#[derive(Clone, Copy)]
struct LexerState {
    row_idx: u32,
    lexer_state: StateID, // state after consuming byte
    byte: Option<u8>,
}

#[derive(Clone)]
pub struct Parser {
    lexer: Lexer,
    grammar: Arc<CGrammar>,
    scratch: Scratch,
    trie_lexer_stack: usize,
    captures: Vec<(String, Vec<u8>)>,
    lexer_stack: Vec<LexerState>,
    rows: Vec<Row>,
    row_infos: Vec<RowInfo>,
    pub(crate) stats: ParserStats,
    last_collapse: usize,
    token_idx: usize,
    byte_idx: usize,
    options: GenGrammarOptions,
    trie_gen_grammar: Option<CSymIdx>,
    trie_gen_grammar_accepting: bool,
}

impl Scratch {
    fn new(grammar: Arc<CGrammar>) -> Self {
        Scratch {
            grammar,
            row_start: 0,
            row_end: 0,
            items: vec![],
            item_props: vec![],
            definitive: true,
        }
    }

    fn new_row(&mut self, pos: usize) {
        self.row_start = pos;
        self.row_end = pos;
    }

    fn row_len(&self) -> usize {
        self.row_end - self.row_start
    }

    fn work_row(&self, allowed_lexemes: SimpleVob) -> Row {
        Row {
            first_item: self.row_start,
            last_item: self.row_end,
            allowed_lexemes,
        }
    }

    #[inline(always)]
    fn ensure_items(&mut self, n: usize) {
        if self.items.len() < n {
            let missing = n - self.items.len();
            self.items.reserve(missing);
            unsafe { self.items.set_len(n) }
        }
    }

    #[inline(always)]
    fn merge_item_origin(&mut self, target_item_idx: usize, origin_item_idx: usize) {
        let origin = self.item_props[origin_item_idx].clone();
        self.item_props[target_item_idx].merge(origin);
    }

    #[inline(always)]
    fn just_add(&mut self, item: Item, origin_item_idx: usize, info: &str) {
        self.ensure_items(self.row_end + 1);
        // SAFETY: we just ensured that there is enough space
        unsafe {
            self.items.as_mut_ptr().add(self.row_end).write(item);
        }
        // self.items[self.row_end] = item;
        if self.definitive {
            if self.item_props.len() <= self.row_end {
                self.item_props.push(ItemProps::default());
            } else {
                self.item_props[self.row_end] = ItemProps::default();
            }
            self.merge_item_origin(self.row_end, origin_item_idx);

            debug!(
                "      addu: {} ({})",
                self.item_to_string(self.row_end),
                info
            );
        }
        self.row_end += 1;
    }

    #[inline(always)]
    fn find_item(&self, item: Item) -> Option<usize> {
        self.items[self.row_start..self.row_end]
            .iter()
            .position(|&x| x == item)
            .map(|x| x + self.row_start)
    }

    fn set_hidden_start(&mut self, item: Item, hidden_start: usize) {
        let idx = self.find_item(item).unwrap();
        self.item_props[idx].hidden_start =
            std::cmp::min(self.item_props[idx].hidden_start, hidden_start);
        debug!(
            "      hidden: {} {}",
            hidden_start,
            self.item_to_string(idx),
        );
    }

    #[inline(always)]
    fn add_unique(&mut self, item: Item, origin_item_idx: usize, info: &str) {
        if let Some(idx) = self.find_item(item) {
            if self.definitive {
                self.merge_item_origin(idx, origin_item_idx);
            }
        } else {
            self.just_add(item, origin_item_idx, info);
        }
    }

    fn item_to_string(&self, idx: usize) -> String {
        let r = item_to_string(&self.grammar, &self.items[idx]);
        if self.definitive {
            let props = &self.item_props[idx];
            format!("{} {}", r, props)
        } else {
            r
        }
    }
}

macro_rules! ensure_internal {
    ($cond:expr, $msg:expr) => {
        ensure!($cond, "Internal error: {}", $msg)
    };
}

impl Parser {
    pub fn new(grammar: Arc<CGrammar>, options: GenGrammarOptions) -> Result<Self> {
        let start = grammar.start();
        let lexer = Lexer::from(grammar.lexer_spec())?;
        let scratch = Scratch::new(Arc::clone(&grammar));
        let lexer_state = lexer.a_dead_state(); // placeholder
        let mut r = Parser {
            grammar,
            lexer,
            trie_lexer_stack: usize::MAX,
            rows: vec![],
            row_infos: vec![],
            captures: vec![],
            scratch,
            stats: ParserStats::default(),
            last_collapse: 0,
            token_idx: 0,
            byte_idx: 0,
            options,
            trie_gen_grammar: None,
            trie_gen_grammar_accepting: false,
            lexer_stack: vec![LexerState {
                row_idx: 0,
                lexer_state,
                byte: None,
            }],
        };
        for rule in r.grammar.rules_of(start).to_vec() {
            r.scratch.add_unique(Item::new(rule, 0), 0, "init");
        }
        debug!("initial push");
        let _ = r.push_row(0, r.scratch.row_start, &Lexeme::bogus());
        ensure_internal!(
            r.num_rows() == 1 && r.rows.len() == 1,
            "initial push failed"
        );
        assert!(r.lexer_stack.len() == 1);
        // set the correct initial lexer state
        // the initial state, shall not allow the SKIP lexeme
        r.rows[0]
            .allowed_lexemes
            .set(LexemeIdx::SKIP.as_usize(), false);
        r.lexer_stack[0].lexer_state = r.lexer.start_state(&r.rows[0].allowed_lexemes, None);
        r.assert_definitive();

        Ok(r)
    }

    pub fn compute_bias_after_gen_grammar(
        &mut self,
        trie: &TokTrie,
        symidx: CSymIdx,
    ) -> (bool, SimpleVob) {
        self.trie_gen_grammar = Some(symidx);
        let r = self.compute_bias(trie, &[]);
        assert!(self.trie_gen_grammar.is_none());
        (self.trie_gen_grammar_accepting, r)
    }

    pub fn compute_bias(&mut self, trie: &TokTrie, start: &[u8]) -> SimpleVob {
        let mut set = trie.alloc_token_set();

        trie.add_bias(self, &mut set, start);
        trie.apply_duplicates(&mut set);

        if set.is_zero() {
            // nothing allowed
            // we're going to be stopped outside - we better flush the lexer
            let _ = self.flush_lexer();
        }

        if start.is_empty() && self.lexer_allows_eos() {
            set.allow_token(trie.eos_token());
        }

        set
    }

    pub fn grammar(&self) -> &CGrammar {
        &self.grammar
    }

    fn after_dots(&self) -> impl Iterator<Item = RuleIdx> + '_ {
        self.curr_row()
            .item_indices()
            .map(|i| self.scratch.items[i].rule_idx())
    }

    fn after_dots_symdata(&self) -> impl Iterator<Item = &CSymbol> + '_ {
        self.after_dots().map(|pos| self.grammar.sym_data_dot(pos))
    }

    fn can_advance_inner(&self) -> bool {
        for data in self.after_dots_symdata() {
            if data.lexeme == Some(LexemeIdx::SKIP) || data.idx == CSymIdx::NULL {
                continue;
            }
            if data.is_terminal || data.gen_grammar.is_some() {
                return true;
            }
        }
        false
    }

    pub fn can_advance(&self) -> bool {
        self.has_pending_lexeme_bytes() || self.can_advance_inner()
    }

    pub fn has_pending_lexeme_bytes(&self) -> bool {
        self.curr_row_bytes().len() > 0
    }

    fn row_is_accepting(&self) -> bool {
        for pos in self.after_dots() {
            let after_dot = self.grammar.sym_idx_dot(pos);
            if after_dot == CSymIdx::NULL {
                let lhs = self.grammar.sym_idx_lhs(pos);
                if lhs == self.grammar.start() {
                    return true;
                }
            }
        }
        false
    }

    pub fn lexer_allows_eos(&mut self) -> bool {
        if self.has_pending_lexeme_bytes() {
            self.lexer.allows_eos(self.lexer_state().lexer_state)
        } else {
            // empty lexemes are not allowed
            false
        }
    }

    fn item_to_string(&self, idx: usize) -> String {
        self.scratch.item_to_string(idx)
    }

    pub fn print_row(&self, row_idx: usize) {
        let row = &self.rows[row_idx];
        println!(
            "row {}; lexer_stack={} top_state={:?}",
            row_idx,
            self.lexer_stack.len(),
            self.lexer_stack.last().unwrap().lexer_state
        );

        println!(
            "  allowed: {}",
            self.lexer_spec().dbg_lexeme_set(&row.allowed_lexemes)
        );

        if row_idx < self.row_infos.len() {
            let info = &self.row_infos[row_idx];
            if info.lexeme.is_bogus() {
                println!("  lexeme: placeholder");
            } else {
                println!("  lexeme: {}", self.lexer_spec().dbg_lexeme(&info.lexeme));
            }
        } else {
            println!("  speculative");
        }
        for i in row.item_indices() {
            println!("  {}", self.item_to_string(i));
        }
    }

    #[inline(always)]
    fn lexer_state(&self) -> LexerState {
        self.lexer_stack[self.lexer_stack.len() - 1]
    }

    #[inline(always)]
    pub fn num_rows(&self) -> usize {
        self.lexer_state().row_idx as usize + 1
    }

    fn pop_lexer_states(&mut self, n: usize) {
        assert!(self.lexer_stack.len() > n);
        unsafe { self.lexer_stack.set_len(self.lexer_stack.len() - n) }
    }

    #[allow(dead_code)]
    pub fn print_stats(&mut self) {
        println!("stats: {:?}", self.stats);
        self.stats = ParserStats::default();
    }

    fn assert_definitive(&self) {
        assert!(self.scratch.definitive);
        if self.num_rows() != self.row_infos.len() {
            panic!(
                "num_rows={} row_infos={}",
                self.num_rows(),
                self.row_infos.len()
            );
        }
    }

    fn get_bytes_and_lexeme_indices(&mut self) -> (Vec<usize>, Vec<u8>) {
        self.assert_definitive();
        let mut indices = vec![];
        let mut allbytes = vec![];
        trace!("get_bytes:");
        for idx in 0..self.row_infos.len() {
            let ri = &self.row_infos[idx];
            trace!("  lexeme: {}", self.lexer_spec().dbg_lexeme(&ri.lexeme));
            let mut bytes = ri.lexeme.visible_bytes().to_vec();
            if bytes.is_empty() && idx == self.num_rows() - 1 {
                bytes = self.curr_row_bytes();
                trace!("    bytes: {:?}", String::from_utf8_lossy(&bytes));
            };
            self.row_infos[idx].start_byte_idx = allbytes.len();
            indices.extend((0..bytes.len()).map(|_| idx));
            allbytes.extend_from_slice(&bytes);
        }
        (indices, allbytes)
    }

    pub fn get_bytes(&mut self) -> Vec<u8> {
        self.get_bytes_and_lexeme_indices().1
    }

    fn item_lhs(&self, item: &Item) -> CSymIdx {
        self.grammar.sym_idx_lhs(item.rule_idx())
    }

    fn item_sym_data(&self, item: &Item) -> &CSymbol {
        self.grammar.sym_data(self.item_lhs(item))
    }

    pub fn hidden_start(&mut self) -> usize {
        let hidden_len = self
            .lexer
            .possible_hidden_len(self.lexer_state().lexer_state);
        if hidden_len == 0 {
            return usize::MAX;
        }
        let last_lexeme_visible_len = self.curr_row_bytes().len() - hidden_len;
        let prefix_len = self.row_infos[self.num_rows() - 1].start_byte_idx;
        prefix_len + last_lexeme_visible_len
    }

    pub fn temperature(&self) -> f32 {
        let mut temp = 0.0f32;
        for data in self.after_dots_symdata() {
            if data.is_terminal {
                temp = temp.max(data.props.temperature);
            }
        }
        if self.options.temperature.is_some() {
            temp = temp.max(self.options.temperature.unwrap());
        }
        temp
    }

    pub fn apply_tokens(
        &mut self,
        trie: &TokTrie,
        tokens: &[TokenId],
        mut num_skip: usize,
    ) -> Result<&'static str> {
        debug!("apply_tokens: {:?}\n  {}", tokens, trie.tokens_dbg(tokens));
        self.assert_definitive();
        // reset token_idx
        for ri in self.row_infos.iter_mut() {
            ri.token_idx_start = usize::MAX;
            ri.token_idx_stop = 0;
        }
        let mut last_lexeme = 0;
        let (indices, grm_bytes) = self.get_bytes_and_lexeme_indices();
        let mut byte_idx = 0;

        for (tok_idx, t) in tokens.iter().enumerate() {
            let tok_bytes = trie.token(*t).to_vec();
            for (idx_within_token, b) in tok_bytes.iter().enumerate() {
                if num_skip > 0 {
                    num_skip -= 1;
                    continue;
                }

                if byte_idx >= grm_bytes.len() {
                    self.token_idx = tok_idx; // save local pointer, in case push_row() uses it
                    self.byte_idx = byte_idx;
                    let row_idx = self.num_rows() - 1;
                    self.row_infos[row_idx].apply_token_idx(tok_idx);
                    debug!(
                        "  before push: {}",
                        self.row_infos.last().unwrap().dbg(self.lexer_spec())
                    );

                    debug!("B: {:?}", *b as char);
                    if !self.try_push_byte_definitive(Some(*b)) {
                        return Ok("parse reject");
                    }

                    // if we didn't push a new row, and are at the end of the current token,
                    // check on max_tokens
                    if idx_within_token == tok_bytes.len() - 1 && row_idx == self.num_rows() - 1 {
                        let info = &self.row_infos[row_idx];
                        let info_tokens = std::cmp::max(
                            0,
                            self.token_idx as isize + 1 - info.token_idx_start as isize,
                        ) as usize;
                        let lex_state = self.lexer_state().lexer_state;
                        let mut limit = trie.alloc_token_set();
                        let mut num_limit = 0;
                        for idx in self.lexer.possible_lexemes(lex_state).iter() {
                            let lex = LexemeIdx::new(idx as usize);
                            let max_tokens = *info.max_tokens.get(&lex).unwrap_or(&usize::MAX);
                            trace!(
                                "  max_tokens: {} max={} info={}",
                                self.lexer_spec().dbg_lexeme(&Lexeme::just_idx(lex)),
                                max_tokens,
                                info_tokens
                            );
                            if info_tokens < max_tokens {
                                limit.allow_token(idx);
                            } else {
                                num_limit += 1;
                            }
                        }
                        if num_limit > 0 {
                            debug!(
                                "  max_tokens limiting to: {}",
                                self.lexer_spec().dbg_lexeme_set(&limit)
                            );
                            let new_state = self.lexer.limit_state_to(lex_state, &limit);
                            if new_state.is_dead() {
                                debug!("  limited everything; forcing EOI");
                                if !self.try_push_byte_definitive(None) {
                                    return Ok("parse reject on max_tokens");
                                }
                            } else {
                                self.lexer_stack.last_mut().unwrap().lexer_state = new_state;
                            }
                        }
                    }

                    let item_count = self.curr_row().item_indices().count();
                    if item_count > MAX_ROW {
                        bail!(
                            "Current row has {} items; max is {}; consider making your grammar left-recursive if it's right-recursive",
                            item_count,
                            MAX_ROW,
                        );
                    }
                    last_lexeme = self.num_rows() - 1;
                } else {
                    loop {
                        self.row_infos[last_lexeme].apply_token_idx(tok_idx);
                        if last_lexeme >= indices[byte_idx] {
                            break;
                        }
                        last_lexeme += 1;
                    }

                    if grm_bytes[byte_idx] != *b {
                        println!(
                            "byte mismatch: {} != {} at {}",
                            grm_bytes[byte_idx], b, last_lexeme
                        );
                        return Ok("static reject");
                    }
                }

                byte_idx += 1;
            }
        }

        self.token_idx = tokens.len();
        while last_lexeme < self.row_infos.len() {
            self.row_infos[last_lexeme].apply_token_idx(self.token_idx);
            last_lexeme += 1;
        }

        for infos in self.row_infos.iter() {
            debug!("  {}", infos.dbg(self.lexer_spec()));
        }

        // self.print_row(self.num_rows() - 1);

        return Ok("");
    }

    pub fn filter_max_tokens(&mut self) {
        self.assert_definitive();

        let mut dst = 0;

        self.row_infos.push(RowInfo {
            lexeme: Lexeme::bogus(),
            start_byte_idx: 0,
            token_idx_start: self.token_idx,
            token_idx_stop: self.token_idx,
            max_tokens: HashMap::default(),
        });

        for idx in 0..self.num_rows() {
            let range = self.rows[idx].item_indices();
            self.rows[idx].first_item = dst;
            for i in range {
                let item = self.scratch.items[i];
                let item_props = &self.scratch.item_props[i];
                let sym_data = self.item_sym_data(&item);
                let max_tokens = sym_data.props.max_tokens;
                if max_tokens != usize::MAX {
                    let start_token_idx = self.row_infos[item.start_pos() + 1].token_idx_start;
                    if self.token_idx - start_token_idx >= max_tokens {
                        debug!(
                            "  remove: {}-{} {}",
                            self.token_idx,
                            start_token_idx,
                            self.item_to_string(i)
                        );
                        continue;
                    }
                }
                self.scratch.items[dst] = item;
                self.scratch.item_props[dst] = item_props.clone();
                dst += 1;
            }
            self.rows[idx].last_item = dst;
        }

        self.row_infos.pop();
    }

    pub fn force_bytes(&mut self) -> Vec<u8> {
        self.assert_definitive();
        trace!("force_bytes lexer_stack {}", self.lexer_stack.len());
        let mut bytes = vec![];
        while let Some(b) = self.forced_byte() {
            debug!("  forced: {:?} 0x{:x}", b as char, b);
            if !self.try_push_byte_definitive(Some(b)) {
                // shouldn't happen?
                debug!("  force_bytes reject {}", b as char);
                break;
            }
            bytes.push(b);
        }
        trace!(
            "force_bytes exit {} lexer_stack={}",
            bytes.len(),
            self.lexer_stack.len()
        );
        bytes
    }

    #[inline(always)]
    fn advance_lexer_or_parser(&mut self, lex_result: LexerResult, curr: LexerState) -> bool {
        match lex_result {
            LexerResult::State(next_state, byte) => {
                // lexer advanced, but no lexeme - fast path
                self.lexer_stack.push(LexerState {
                    row_idx: curr.row_idx,
                    lexer_state: next_state,
                    byte: Some(byte),
                });
                true
            }
            LexerResult::Error => false,
            LexerResult::Lexeme(pre_lexeme) => self.advance_parser(pre_lexeme),
        }
    }

    fn trie_started_inner(&mut self) {
        // debug!("trie_started: rows={} lexer={}", self.num_rows(), self.lexer_stack.len());
        self.assert_definitive();
        self.trie_lexer_stack = self.lexer_stack.len();
        self.scratch.definitive = false;
    }

    fn trie_finished_inner(&mut self) {
        // debug!("trie_finished: rows={} lexer={}", self.num_rows(), self.lexer_stack.len());
        assert!(self.scratch.definitive == false);
        assert!(self.row_infos.len() <= self.num_rows());
        // clean up stack
        self.pop_lexer_states(self.lexer_stack.len() - self.trie_lexer_stack);
        self.scratch.definitive = true;
        self.assert_definitive();
    }

    fn run_speculative<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.trie_started_inner();
        let r = f(self);
        self.trie_finished_inner();
        r
    }

    pub fn is_accepting(&mut self) -> bool {
        self.run_speculative(|s| s.flush_lexer() && s.row_is_accepting())
    }

    pub fn try_push_byte_definitive(&mut self, byte: Option<u8>) -> bool {
        assert!(self.scratch.definitive);

        let curr = self.lexer_state();
        let row = &self.rows[curr.row_idx as usize];

        let res = if byte.is_none() {
            let lexeme = self.lexer.force_lexeme_end(curr.lexer_state);
            if lexeme.is_error() {
                debug!(
                    "    lexer fail on forced end; allowed: {}",
                    self.lexer_spec().dbg_lexeme_set(&row.allowed_lexemes)
                );
            }
            lexeme
        } else {
            self.stats.definitive_bytes += 1;
            self.lexer
                .advance(curr.lexer_state, byte.unwrap(), self.scratch.definitive)
        };

        if res.is_error() {
            debug!(
                "  lexer fail; allowed: {}",
                self.lexer_spec().dbg_lexeme_set(&row.allowed_lexemes)
            );
        }

        self.advance_lexer_or_parser(res, curr)
    }

    fn curr_row(&self) -> &Row {
        &self.rows[self.lexer_state().row_idx as usize]
    }

    pub fn model_variables(&mut self) -> Vec<ModelVariable> {
        // this can be used in future to allow "end-of-turn" special token and the like
        self.run_speculative(|s| {
            let mut vars = vec![];
            if s.flush_lexer() {
                for sym_data in s.after_dots_symdata() {
                    if let Some(ref mv) = sym_data.props.model_variable {
                        if !vars.contains(mv) {
                            vars.push(mv.clone());
                        }
                    }
                }
            }
            vars
        })
    }

    fn forced_byte(&mut self) -> Option<u8> {
        if self.is_accepting() {
            debug!("  in accept state, not forcing");
            return None;
        }

        // self.print_row(self.num_rows() - 1);

        self.run_speculative(|s| {
            let mut byte_sym = None;
            for b in 0..=255 {
                if s.try_push_byte(b) {
                    s.pop_bytes(1);
                    // debug!("  forced: {:?}", b as char);
                    if byte_sym.is_some() {
                        // debug!("  forced multiple");
                        return None; // more than one option
                    } else {
                        byte_sym = Some(b);
                    }
                }
            }
            byte_sym
        })
    }

    /// Advance the parser as if the current lexeme (if any)
    /// finished right here.
    /// Returns true if the parser was able to advance (or there were no pending bytes for a lexeme).
    fn flush_lexer(&mut self) -> bool {
        if !self.has_pending_lexeme_bytes() {
            return true;
        }
        let curr = self.lexer_state();
        let lex_result = self.lexer.try_lexeme_end(curr.lexer_state);
        self.advance_lexer_or_parser(lex_result, curr)
    }

    pub fn maybe_gen_grammar(&mut self) -> Option<(String, CSymIdx, GenGrammarOptions)> {
        self.assert_definitive();
        let mut res: Option<GenGrammarOptions> = None;
        let mut res_idx = None;
        let mut gen_grm = vec![];
        for pos in self.after_dots() {
            let idx = self.grammar.sym_idx_dot(pos);
            let sym_data = self.grammar.sym_data_dot(pos);
            if let Some(ref gg) = sym_data.gen_grammar {
                // break ties by preferring the one with the lowest grammar number
                if res.is_none() || res.as_ref().unwrap().grammar.0 > gg.grammar.0 {
                    res = Some(gg.clone());
                    res_idx = Some(idx);
                }
                gen_grm.push(idx);
            } else if sym_data.is_terminal {
                gen_grm.push(idx);
            }
        }

        if res.is_none() {
            return None;
        }

        let msg = if gen_grm.len() > 1 {
            format!(
                "ambiguity between GenGrammar and terminals {:?}",
                gen_grm
                    .iter()
                    .map(|&x| self.grammar.sym_name(x))
                    .collect::<Vec<_>>()
            )
        } else {
            String::new()
        };

        Some((msg, res_idx.unwrap(), res.unwrap()))
    }

    fn flush_gen_grammar(&mut self) {
        if let Some(idx) = self.trie_gen_grammar.take() {
            let r = self.scan_gen_grammar_inner(idx, vec![]);
            self.trie_gen_grammar_accepting = r && self.row_is_accepting();
        }
    }

    pub fn scan_gen_grammar(&mut self, symidx: CSymIdx, inner_bytes: Vec<u8>) -> bool {
        self.assert_definitive();
        self.scan_gen_grammar_inner(symidx, inner_bytes)
    }

    fn scan_gen_grammar_inner(&mut self, symidx: CSymIdx, inner_bytes: Vec<u8>) -> bool {
        debug!("  scan gen_grammar: {}", self.grammar.sym_name(symidx));

        self.scratch.new_row(self.curr_row().last_item);

        for idx in self.curr_row().item_indices() {
            let item = self.scratch.items[idx];
            let sidx = self.grammar.sym_idx_dot(item.rule_idx());
            if sidx == symidx {
                self.scratch
                    .add_unique(item.advance_dot(), idx, "gen_grammar");
            }
        }

        assert!(self.scratch.row_len() > 0);

        let lexeme = Lexeme::new(LexemeIdx::SKIP, inner_bytes, 0);

        let r = self.push_row(self.num_rows(), self.scratch.row_start, &lexeme);
        if r {
            debug!("  gen_grammar OK");
            let lexer_state = self.lexer_state_for_added_row(lexeme, None);
            self.lexer_stack.push(lexer_state);
            true
        } else {
            debug!("  gen_grammar failed!");
            false
        }
    }

    pub fn scan_model_variable(&mut self, mv: ModelVariable) -> bool {
        self.assert_definitive(); // ???

        let lexer_eos = self.lexer_allows_eos();

        debug!("  scan mv: {:?}; lexer_eos={}", mv, lexer_eos);

        if !self.flush_lexer() {
            debug!("  flush_lexer() failed");
            return false;
        }

        debug!("  flush_lexer() OK");

        if mv == ModelVariable::eos_token() {
            if lexer_eos {
                return true;
            }
            // This is really for EOS tokens in the middle of the grammar
            // that need to be eaten; so don't check for accepting state here
            // if self.is_accepting() {
            //     return true;
            // }
        }

        self.scratch.new_row(self.curr_row().last_item);

        for idx in self.curr_row().item_indices() {
            let item = self.scratch.items[idx];
            let sym_data = self.grammar.sym_data_dot(item.rule_idx());
            if let Some(ref mv2) = sym_data.props.model_variable {
                if mv == *mv2 {
                    self.scratch
                        .add_unique(item.advance_dot(), idx, "scan_model_variable");
                }
            }
        }

        if self.scratch.row_len() == 0 {
            debug!("  scan_model_variable: no items");
            false
        } else {
            let r = self.push_row(self.num_rows(), self.scratch.row_start, &Lexeme::bogus());
            debug!("  scan_model_variable: {}", r);
            r
        }
    }

    // this just copies current row
    fn scan_skip_lexeme(&mut self, lexeme: &Lexeme) -> bool {
        let src = self.curr_row().item_indices();
        let allowed_lexemes = self.curr_row().allowed_lexemes.clone();
        let n = src.len();
        if n == 0 {
            return false;
        }
        self.scratch.ensure_items(src.end + n + 100);
        self.scratch.new_row(src.end);

        for i in src {
            self.scratch
                .just_add(self.scratch.items[i], i, "skip_lexeme");
        }

        // note that we pass 'row_end' not 'row_start' as the agenda pointer
        // this will skip processing any items, and only push the row
        let push_res = self.push_row(self.num_rows(), self.scratch.row_end, lexeme);
        assert!(push_res);
        let added_row_idx = self.num_rows();
        // the allowed_lexemes were not computed correctly due to us messing
        // with agenda pointer above
        self.rows[added_row_idx].allowed_lexemes = allowed_lexemes;
        if self.scratch.definitive {
            self.row_infos[added_row_idx].max_tokens =
                self.row_infos[added_row_idx - 1].max_tokens.clone();
        }
        true
    }

    // lexeme body only used for captures (in definitive mode)
    // and debugging (lexeme.idx used always)
    fn scan(&mut self, lexeme: &Lexeme) -> bool {
        let row_idx = self.num_rows() - 1;
        let last = self.rows[row_idx].last_item;
        let mut i = self.rows[row_idx].first_item;
        let n = last - i;
        self.scratch.ensure_items(last + n + 100);
        self.scratch.new_row(last);

        if self.scratch.definitive {
            debug!(
                "  scan: {} at {} (spec: {:?})",
                self.lexer_spec().dbg_lexeme(&lexeme),
                row_idx,
                self.lexer_spec().lexeme_spec(lexeme.idx),
            );
        }

        while i < last {
            let item = self.scratch.items[i];
            let sym = self.grammar.sym_data_dot(item.rule_idx());
            if sym.lexeme == Some(lexeme.idx) {
                self.scratch.just_add(item.advance_dot(), i, "scan");
            }
            i += 1;
        }
        self.push_row(self.num_rows(), self.scratch.row_start, lexeme)
    }

    pub fn captures(&self) -> &[(String, Vec<u8>)] {
        &self.captures
    }

    // lexeme only used for captures (in definitive mode)
    #[inline(always)]
    fn push_row(&mut self, curr_idx: usize, mut agenda_ptr: usize, lexeme: &Lexeme) -> bool {
        let mut allowed_lexemes = self.lexer_spec().alloc_lexeme_set();
        let mut max_tokens = vec![];

        while agenda_ptr < self.scratch.row_end {
            let item_idx = agenda_ptr;
            let item = self.scratch.items[agenda_ptr];
            agenda_ptr += 1;
            if self.scratch.definitive {
                debug!("    agenda: {}", self.item_to_string(item_idx));
            }

            let rule = item.rule_idx();
            let after_dot = self.grammar.sym_idx_dot(rule);

            if after_dot == CSymIdx::NULL {
                let flags = self.grammar.sym_flags_lhs(rule);
                let lhs = self.grammar.sym_idx_lhs(rule);

                if self.scratch.definitive && flags.stop_capture() {
                    let var_name = self
                        .grammar
                        .sym_data(lhs)
                        .props
                        .stop_capture_name
                        .as_ref()
                        .unwrap();
                    let bytes = lexeme.hidden_bytes();
                    self.captures.push((var_name.clone(), bytes.to_vec()));
                }

                if self.scratch.definitive && flags.capture() {
                    let var_name = self
                        .grammar
                        .sym_data(lhs)
                        .props
                        .capture_name
                        .as_ref()
                        .unwrap();
                    let mut bytes = Vec::new();
                    let capture_start = item.start_pos();
                    if capture_start < curr_idx {
                        bytes = self.row_infos[capture_start..curr_idx]
                            .iter()
                            .map(|ri| ri.lexeme.visible_bytes())
                            .collect::<Vec<_>>()
                            .concat();
                    }
                    bytes.extend_from_slice(lexeme.visible_bytes());
                    debug!(
                        "      capture: {} {:?}",
                        var_name,
                        String::from_utf8_lossy(&bytes)
                    );
                    self.captures.push((var_name.clone(), bytes));
                }

                if item.start_pos() < curr_idx {
                    // if item.start_pos() == curr_idx, then we handled it below in the nullable check
                    for i in self.rows[item.start_pos()].item_indices() {
                        let item = self.scratch.items[i];
                        if self.grammar.sym_idx_dot(item.rule_idx()) == lhs {
                            self.scratch.add_unique(item.advance_dot(), i, "complete");
                        }
                    }
                }
            } else {
                let sym_data = self.grammar.sym_data(after_dot);
                if let Some(lx) = sym_data.lexeme {
                    allowed_lexemes.set(lx.as_usize(), true);
                    if self.scratch.definitive {
                        max_tokens.push((lx, sym_data.props.max_tokens));
                    }
                }
                if sym_data.is_nullable {
                    self.scratch
                        .add_unique(item.advance_dot(), item_idx, "null");
                    if self.scratch.definitive && sym_data.props.capture_name.is_some() {
                        // nullable capture
                        let var_name = sym_data.props.capture_name.as_ref().unwrap();
                        debug!("      capture: {} NULL", var_name);
                        self.captures.push((var_name.clone(), vec![]));
                    }
                }
                for rule in &sym_data.rules {
                    let new_item = Item::new(*rule, curr_idx);
                    self.scratch.add_unique(new_item, item_idx, "predict");
                }
                // TODO the hidden stuff is no longer used
                if self.scratch.definitive && sym_data.props.hidden {
                    for rule in &sym_data.rules {
                        let new_item = Item::new(*rule, curr_idx);
                        self.scratch.set_hidden_start(new_item, curr_idx);
                    }
                }
            }
        }

        let row_len = self.scratch.row_len();

        self.stats.rows += 1;

        if row_len == 0 {
            false
        } else {
            self.stats.all_items += row_len;

            allowed_lexemes.set(LexemeIdx::SKIP.as_usize(), true);

            if self.scratch.definitive {
                debug!(
                    "  push row: {}",
                    self.lexer_spec().dbg_lexeme_set(&allowed_lexemes),
                );
            }

            let idx = self.num_rows();
            let row = self.scratch.work_row(allowed_lexemes);
            if self.rows.len() == 0 || self.rows.len() == idx {
                self.rows.push(row);
            } else {
                self.rows[idx] = row;
            }

            if self.scratch.definitive {
                if self.row_infos.len() > idx {
                    self.row_infos.drain(idx..);
                }
                let mut max_tokens_map = HashMap::default();
                for (lx, mx) in max_tokens {
                    if let Some(ex) = max_tokens_map.get(&lx) {
                        if *ex < mx {
                            max_tokens_map.insert(lx, mx);
                        }
                    } else {
                        max_tokens_map.insert(lx, mx);
                    }
                }
                let mut to_remove = vec![];
                for (lx, mx) in max_tokens_map.iter() {
                    if *mx == usize::MAX {
                        to_remove.push(*lx);
                    }
                }
                for lx in to_remove {
                    max_tokens_map.remove(&lx);
                }
                self.row_infos.push(RowInfo {
                    lexeme: Lexeme::bogus(),
                    token_idx_start: self.token_idx,
                    token_idx_stop: self.token_idx,
                    start_byte_idx: self.byte_idx,
                    max_tokens: max_tokens_map,
                });
                // debug!("  push: {idx} {} {}", self.rows.len(), self.row_infos.len());
            }

            true
        }
    }

    #[inline(always)]
    fn curr_row_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![];
        let row_idx = self.num_rows() - 1;
        for back in self.lexer_stack.iter().rev() {
            if back.row_idx as usize != row_idx {
                break;
            }
            if let Some(b) = back.byte {
                bytes.push(b);
            }
        }
        bytes.reverse();
        bytes
    }

    fn lexer_spec(&self) -> &LexerSpec {
        self.grammar.lexer_spec()
    }

    #[inline(always)]
    fn mk_lexeme(&self, byte: Option<u8>, pre_lexeme: PreLexeme) -> Lexeme {
        let mut bytes = self.curr_row_bytes();
        if byte.is_some() {
            bytes.push(byte.unwrap());
        }
        Lexeme::new(pre_lexeme.idx, bytes, pre_lexeme.hidden_len)
    }

    fn has_forced_bytes(&self, allowed_lexemes: &SimpleVob, bytes: &[u8]) -> bool {
        // note that this is also used when computing token mask
        if allowed_lexemes.is_zero() {
            return false;
        }
        for lexeme_idx in allowed_lexemes.iter() {
            let lex_spec = &self.lexer_spec().lexemes[lexeme_idx as usize];
            if !lex_spec.has_forced_bytes(bytes) {
                return false;
            }
        }
        // debug!("   forced ok {:?}", String::from_utf8_lossy(bytes));
        true
    }

    #[inline(always)]
    fn lexer_state_for_added_row(
        &mut self,
        lexeme: Lexeme,
        transition_byte: Option<u8>,
    ) -> LexerState {
        // note, that while self.rows[] is updated, the lexer stack is not
        // so the last added row is at self.num_rows(), and not self.num_rows() - 1
        let added_row = self.num_rows();
        let added_row_lexemes = &self.rows[added_row].allowed_lexemes;
        let no_hidden = LexerState {
            row_idx: added_row as u32,
            lexer_state: self.lexer.start_state(added_row_lexemes, transition_byte),
            byte: transition_byte,
        };
        if self.scratch.definitive {
            // save lexeme at the last row, before we mess with the stack
            self.row_infos[added_row - 1].lexeme = lexeme;
            debug!(
                "lex: re-start {:?} (via {:?}); allowed: {}",
                no_hidden.lexer_state,
                transition_byte.map(|b| b as char),
                self.lexer_spec().dbg_lexeme_set(added_row_lexemes)
            );
        }
        no_hidden
    }

    #[inline(always)]
    fn handle_hidden_bytes(
        &mut self,
        no_hidden: LexerState,
        lexeme_byte: Option<u8>,
        pre_lexeme: PreLexeme,
    ) {
        // greedy lexers don't have stop tokens
        assert!(!self.lexer_spec().greedy);

        let added_row_lexemes = &self.rows[self.num_rows()].allowed_lexemes;

        // make sure we have a real lexeme
        let lexeme = self.mk_lexeme(lexeme_byte, pre_lexeme);

        let hidden_bytes = lexeme.hidden_bytes();
        assert!(hidden_bytes.len() == pre_lexeme.hidden_len);

        if self.scratch.definitive {
            trace!(
                "  allowed lexemes: {}",
                self.lexer_spec().dbg_lexeme_set(added_row_lexemes)
            );
            trace!("  hidden: {:?}", String::from_utf8_lossy(&hidden_bytes));
        }

        if self.has_forced_bytes(added_row_lexemes, &hidden_bytes) {
            if self.scratch.definitive {
                trace!("  hidden forced");
            }
            let mut lexer_state = self.lexer.start_state(added_row_lexemes, None);
            // if the bytes are forced, we just advance the lexer
            // by replacing the top lexer states
            self.pop_lexer_states(hidden_bytes.len() - 1);
            self.stats.hidden_bytes += hidden_bytes.len();
            for b in hidden_bytes {
                match self.lexer.advance(lexer_state, *b, self.scratch.definitive) {
                    LexerResult::State(next_state, _) => {
                        lexer_state = next_state;
                    }
                    LexerResult::Error => panic!("hidden byte failed; {:?}", hidden_bytes),
                    LexerResult::Lexeme(lex) => panic!(
                        "hidden byte produced lexeme {}",
                        self.lexer_spec().dbg_lexeme(&Lexeme::just_idx(lex.idx))
                    ),
                }
                self.lexer_stack.push(LexerState {
                    lexer_state,
                    byte: Some(*b),
                    ..no_hidden
                });
            }
        } else {
            if self.scratch.definitive {
                // set it up for matching after backtrack
                self.lexer_stack.push(LexerState {
                    lexer_state: self.lexer.start_state(added_row_lexemes, None),
                    byte: None,
                    ..no_hidden
                });
            } else {
                // prevent any further matches in this branch
                self.lexer_stack.push(LexerState {
                    lexer_state: self.lexer.a_dead_state(),
                    byte: None,
                    ..no_hidden
                });
            }
        }
    }

    /// Advance the parser with given lexeme_idx.
    /// lexer_state is state *after* consuming the byte.
    /// It either initial lexer states for lazy lexers,
    /// or lexer_initial_state+byte for greedy lexers.
    /// lexer_byte is the byte that led to producing the lexeme.
    #[inline(always)]
    fn advance_parser(&mut self, pre_lexeme: PreLexeme) -> bool {
        let transition_byte = if pre_lexeme.byte_next_row {
            pre_lexeme.byte
        } else {
            None
        };
        let lexeme_byte = if pre_lexeme.byte_next_row {
            None
        } else {
            pre_lexeme.byte
        };
        let lexeme_idx = pre_lexeme.idx;

        let lexeme = if self.scratch.definitive {
            self.mk_lexeme(lexeme_byte, pre_lexeme)
        } else {
            Lexeme::just_idx(lexeme_idx)
        };

        let scan_res = if lexeme.idx == LexemeIdx::SKIP {
            self.scan_skip_lexeme(&lexeme)
        } else {
            self.scan(&lexeme)
        };

        if scan_res {
            let mut no_hidden = self.lexer_state_for_added_row(lexeme, transition_byte);

            if pre_lexeme.hidden_len > 0 {
                self.handle_hidden_bytes(no_hidden, lexeme_byte, pre_lexeme);
            } else {
                if pre_lexeme.byte_next_row && no_hidden.lexer_state.is_dead() {
                    return false;
                }
                if let Some(b) = transition_byte {
                    if let Some(second_lexeme) = self
                        .lexer
                        .check_for_single_byte_lexeme(no_hidden.lexer_state, b)
                    {
                        if self.scratch.definitive {
                            debug!("single byte lexeme: {:?}", second_lexeme);
                        }
                        no_hidden.byte = None;
                        self.lexer_stack.push(no_hidden);

                        // disallow recursion depth > 2
                        assert!(pre_lexeme.byte_next_row);
                        assert!(!second_lexeme.byte_next_row);

                        let r = self.advance_parser(second_lexeme);
                        if r {
                            let new_top = self.lexer_stack.pop().unwrap();
                            *self.lexer_stack.last_mut().unwrap() = new_top;
                            return true;
                        } else {
                            self.lexer_stack.pop();
                            return false;
                        }
                    }
                }
                self.lexer_stack.push(no_hidden);
            }
            if self.scratch.definitive {
                self.assert_definitive();
            }
            true
        } else {
            if self.scratch.definitive {
                debug!("  scan failed");
            }
            false
        }
    }
}

impl Recognizer for Parser {
    fn pop_bytes(&mut self, num: usize) {
        self.pop_lexer_states(num);
    }

    fn collapse(&mut self) {
        // this actually means "commit" - can no longer backtrack past this point

        if false {
            for idx in self.last_collapse..self.num_rows() {
                self.print_row(idx);
            }
        }
        self.last_collapse = self.num_rows();
    }

    fn special_allowed(&mut self, _tok: SpecialToken) -> bool {
        // handle EOS logic outside
        unreachable!("special_allowed")

        // if self
        //     .model_variables()
        //     .contains(&ModelVariable::SpecialToken(tok))
        // {
        //     true
        // } else if tok == SpecialToken::EndOfSentence {
        //     self.is_accepting() || self.lexer_allows_eos()
        // } else {
        //     false
        // }
    }

    fn trie_started(&mut self) {
        self.trie_started_inner();
        self.flush_gen_grammar();
    }

    fn trie_finished(&mut self) {
        self.trie_finished_inner();
    }

    #[inline(always)]
    fn try_push_byte(&mut self, byte: u8) -> bool {
        assert!(!self.scratch.definitive);
        let lexer_logging = false;
        self.stats.lexer_ops += 1;
        let curr = self.lexer_state();
        let res = self.lexer.advance(curr.lexer_state, byte, lexer_logging);
        self.advance_lexer_or_parser(res, curr)
    }
}

fn item_to_string(g: &CGrammar, item: &Item) -> String {
    format!(
        "{} @{}",
        g.rule_to_string(item.rule_idx()),
        item.start_pos(),
    )
}
