//lexer_atnsimulator_debug    = false
//lexer_atnsimulator_dfadebug = false
//
//lexer_atnsimulator_min_dfaedge = 0
//lexer_atnsimulator_max_dfaedge = 127
//lexer_atnsimulator_match_calls = 0


use std::cell::Cell;
use std::convert::TryFrom;
use std::io::Write;
use std::ops::{Add, Deref, DerefMut, Index};
use std::rc::Rc;
use std::sync::Arc;
use std::usize;

use crate::atn::ATN;
use crate::atn_config::{ATNConfig, ATNConfigType};
use crate::atn_config_set::ATNConfigSet;
use crate::atn_simulator::{BaseATNSimulator, IATNSimulator};
use crate::atn_state::{ATNState, ATNStateType};
use crate::atn_state::ATNStateType::RuleStopState;
use crate::char_stream::CharStream;
use crate::dfa::DFA;
use crate::dfa_state::{DFAState, DFAStateRef};
use crate::errors::ANTLRError;
use crate::errors::ANTLRError::LexerNoAltError;
use crate::int_stream::{EOF, IntStream};
use crate::lexer::{BaseLexer, Lexer, LEXER_MAX_CHAR_VALUE, LEXER_MIN_CHAR_VALUE, LexerPosition, LexerRecog};
use crate::lexer_action_executor::LexerActionExecutor;
use crate::parser_rule_context::empty_ctx;
use crate::prediction_context::{PREDICTION_CONTEXT_EMPTY_RETURN_STATE, PredictionContext,
                                PredictionContextCache};
use crate::prediction_context::EMPTY_PREDICTION_CONTEXT;
use crate::recognizer::Recognizer;
use crate::token::TOKEN_EOF;
use crate::token_source::TokenSource;
use crate::transition::{ActionTransition, PredicateTransition, RuleTransition, Transition, TransitionType};

pub const ERROR_DFA_STATE_REF: DFAStateRef = usize::MAX;

pub trait ILexerATNSimulator: IATNSimulator {
    fn reset(&mut self);
    fn match_token(
        &mut self,
        mode: usize,
//        input:&mut dyn CharStream,
        lexer: &mut dyn Lexer,
    ) -> Result<isize, ANTLRError>;
    fn get_char_position_in_line(&self) -> isize;
    fn set_char_position_in_line(&mut self, column: isize);
    fn get_line(&self) -> isize;
    fn set_line(&mut self, line: isize);
    fn get_text(&self, input: &dyn CharStream) -> String;
    fn consume(&self, input: &mut dyn CharStream);
    fn recover(&mut self, _re: ANTLRError, input: &mut dyn CharStream) {
        if input.la(1) != EOF {
            self.consume(input)
        }
    }
}

pub struct LexerATNSimulator {
    base: BaseATNSimulator,

    prediction_mode: isize,
    //    merge_cache: DoubleDict,
    start_index: isize,
    pub(crate) current_pos: Rc<LexerPosition>,
    mode: usize,
    prev_accept: SimState,
    pub lexer_action_executor: Option<Box<LexerActionExecutor>>,
}

impl ILexerATNSimulator for LexerATNSimulator {
    fn reset(&mut self) {
        self.prev_accept.reset()
    }

    fn match_token(
        &mut self,
        mode: usize,
//        input:&mut dyn CharStream,
        lexer: &mut dyn Lexer,
    ) -> Result<isize, ANTLRError> {
        self.mode = mode;
        let mark = lexer.get_input_stream().mark();
//        println!("start matching on mode {}",mode);
        let result = (|| {
            self.start_index = lexer.get_input_stream().index();
            self.prev_accept.reset();
            let temp = self.decision_to_dfa();
            let dfa = temp.get(mode as usize)
                .ok_or(ANTLRError::IllegalStateError("invalid mode".into()))?;

            let s0 = dfa.s0.read().unwrap().as_ref().copied();
            match s0 {
                None => self.match_atn(lexer),
                Some(s0) => self.exec_atn(s0, lexer),
                //                Err(_) => panic!("dfa rwlock error")
            }
        })();
        lexer.get_input_stream().release(mark);
        result
    }

    fn get_char_position_in_line(&self) -> isize {
        self.current_pos.char_position_in_line.get()
    }

    fn set_char_position_in_line(&mut self, column: isize) {
        self.current_pos.char_position_in_line.set(column)
    }

    fn get_line(&self) -> isize {
        self.current_pos.line.get()
    }

    fn set_line(&mut self, line: isize) {
        self.current_pos.char_position_in_line.set(line)
    }

    fn get_text(&self, _input: &dyn CharStream) -> String {
        unimplemented!()
    }

    fn consume(&self, _input: &mut dyn CharStream) {
        let ch = _input.la(1);
        if char::try_from(ch as u32) == Ok('\n') {
            self.current_pos.line.update(|x| x + 1);
            self.current_pos.char_position_in_line.set(0);
        } else {
            self.current_pos.char_position_in_line.update(|x| x + 1);
        }
        _input.consume();
    }

//    fn get_recog(&self) -> Rc<RefCell<Box<Recognizer>>>{
//        Rc::clone(&self.recog)
//    }
}

impl IATNSimulator for LexerATNSimulator {
    fn shared_context_cache(&self) -> Arc<PredictionContextCache> {
        self.base.shared_context_cache()
    }

    fn atn(&self) -> &ATN {
        self.base.atn()
    }

    fn decision_to_dfa(&self) -> &Vec<DFA> {
        self.base.decision_to_dfa()
    }
}

pub const MIN_DFA_EDGE: isize = 0;
pub const MAX_DFA_EDGE: isize = 127;

impl LexerATNSimulator {
    pub fn new_lexer_atnsimulator(
        atn: Arc<ATN>,
        decision_to_dfa: Arc<Vec<DFA>>,
        shared_context_cache: Arc<PredictionContextCache>,
    ) -> LexerATNSimulator {
        LexerATNSimulator {
            base: BaseATNSimulator::new_base_atnsimulator(atn, decision_to_dfa, shared_context_cache),
//            recog: Rc::new(RefCell::new(recog)),
            prediction_mode: 0,
            start_index: 0,
            current_pos: Rc::new(LexerPosition { line: Cell::new(0), char_position_in_line: Cell::new(0) }),
            mode: 0,
            prev_accept: SimState::new(),
            lexer_action_executor: None,
        }
    }

//    fn copy_state(&self, _simulator: &mut LexerATNSimulator) {
//        unimplemented!()
//    }

    fn match_atn(&mut self, lexer: &mut dyn Lexer) -> Result<isize, ANTLRError> {
        assert!(self.mode >= 0);
//        println!("\n---start matching");
        //        let start_state = self.atn().mode_to_start_state.get(self.mode as usize).ok_or(ANTLRError::IllegalStateError("invalid mode".into()))?;
        let atn = self.atn();
        let start_state = *atn.mode_to_start_state
            .get(self.mode as usize)
            .ok_or(ANTLRError::IllegalStateError("invalid mode".into()))?;

        let _old_mode = self.mode;
        let mut s0_closure = self.compute_start_state(atn.states[start_state].as_ref(), lexer);
        let _supress_edge = s0_closure.has_semantic_context();
        s0_closure.set_has_semantic_context(false);

        let next_state = self.add_dfastate(&mut self.get_dfa().states.write().unwrap(), s0_closure);
        if !_supress_edge {
            *self.get_dfa().s0.write().unwrap() = Some(next_state);
        }

        self.exec_atn(next_state, lexer)
    }

    //    fn get_dfa_state(&self, state_number: DFAStateRef) -> Box<dyn Deref<Target=&DFAState>>{
    //        let dfa = self.get_dfa();
    //
    //        dfa.states.read().unwrap()
    //    }

    fn exec_atn(
        &mut self,
//        input: &'a mut dyn CharStream,
        ds0: DFAStateRef,
        lexer: &mut dyn Lexer,
    ) -> Result<isize, ANTLRError> {
        //        if self.get_dfa().states.read().unwrap().get(ds0).unwrap().is_accept_state{
        self.capture_sim_state(lexer.get_input_stream(), ds0);
        //        }

        let mut symbol = lexer.get_input_stream().la(1);
        let mut s = ds0;
        loop {
            let target = self.get_existing_target_state(s, symbol);
            let target = target.unwrap_or(self.compute_target_state(s, symbol, lexer));
            //              let target = dfastates.deref().get(s).unwrap() ;x


            if target == ERROR_DFA_STATE_REF {
                break;
            }
//            println!(" --- target computed {:?}", self.get_dfa().states.read().unwrap()[target].configs.configs.iter().map(|it|it.get_state()).collect::<Vec<_>>());

            if symbol != EOF {
                self.consume(lexer.get_input_stream())
            }

            if self.capture_sim_state(lexer.get_input_stream(), target) {
                if symbol == EOF {
                    break;
                }
            }

            symbol = lexer.get_input_stream().la(1);

            s = target;
        }
        let _last = self.get_dfa().states.read().unwrap().get(s).unwrap();

        self.fail_or_accept(symbol, lexer)
    }

    fn get_existing_target_state(&self, _s: DFAStateRef, t: isize) -> Option<DFAStateRef> {
        if t < MIN_DFA_EDGE || t > MAX_DFA_EDGE {
            return None;
        }

        self.get_dfa()
            .states
            .read().unwrap()
            .get(_s).unwrap()
            .edges
            .get((t - MIN_DFA_EDGE) as usize)
            .and_then(|x| match x {
                0 => None,
                x => Some(x),
            })
            .copied()
    }

    fn compute_target_state(&self, _s: DFAStateRef, _t: isize, lexer: &mut dyn Lexer) -> DFAStateRef {
        let states = self.get_dfa().states.read().unwrap();

        let mut reach = ATNConfigSet::new_ordered_atnconfig_set();
        self.get_reachable_config_set(
            &states,
//            _input,
            &states.get(_s).unwrap().configs,
            &mut reach,
            _t,
            lexer,
        );
//        println!(" --- target computed {:?}", reach.configs.iter().map(|it|it.get_state()).collect::<Vec<_>>());


        drop(states);
        let mut states = self.get_dfa().states.write().unwrap();
        if reach.is_empty() {
            if !reach.has_semantic_context() {
                self.add_dfaedge(states.get_mut(_s).unwrap(), _t, ERROR_DFA_STATE_REF);
            }
            return ERROR_DFA_STATE_REF;
        }

        let supress_edge = reach.has_semantic_context();
        reach.set_has_semantic_context(false);
        let to = self.add_dfastate(&mut states, Box::new(reach));
        if !supress_edge {
            let from = states.get_mut(_s).unwrap();
            self.add_dfaedge(from, _t, to);
        }
//        println!("target state computed from {:?} to {:?} on symbol {}", _s, to, char::try_from(_t as u32).unwrap());

        to
        //        states.get(to).unwrap()
    }

    fn get_reachable_config_set<V>(
        &self,
        _states: &V,
//        _input: &mut dyn CharStream,
        _closure: &ATNConfigSet,
        _reach: &mut ATNConfigSet,
        _t: isize,
        lexer: &mut dyn Lexer,
    ) where
        V: Deref<Target=Vec<DFAState>>,
    {
        let mut skip_alt = 0;
//        println!(" --- source {:?}", _closure.configs.iter().map(|it|it.get_state()).collect::<Vec<_>>());
        for config in _closure.get_items() {
            let current_alt_reached_accept_state = config.get_alt() == skip_alt;
            if current_alt_reached_accept_state {
                if let ATNConfigType::LexerATNConfig {
                    passed_through_non_greedy_decision: true, ..
                } = config.get_type()
                {
                    continue;
                }
            }
            let atn_state = self.atn().states[config.get_state()].as_ref();
            for tr in atn_state.get_transitions() {
                if let Some(target) = tr.get_reachable_target(_t) {
                    let exec = config.get_lexer_executor()
                        .map(|x| x.clone().fix_offset_before_match(lexer.get_input_stream().index() - self.start_index));

                    let new = config.cloned_with_new_exec(self.atn().states[target].as_ref(), exec);
                    if self.closure(
                        new,
                        _reach,
                        current_alt_reached_accept_state,
                        true,
                        _t == EOF,
                        lexer,
                    ) {
                        skip_alt = config.get_alt();
                        break;
                    }
                }
            }
        }
    }

//    fn get_reachable_target<T>(&self, states: &T, _trans: &Transition, _t: isize) -> &ATNState
//    where
//        T: Deref<Target = Vec<DFAState>>,
//    {
//        unimplemented!()
//    }

    fn fail_or_accept(&mut self, _t: isize, lexer: &mut dyn Lexer) -> Result<isize, ANTLRError> {
//        println!("fail_or_accept");
        if let Some(state) = self.prev_accept.dfa_state {
//            let lexer_action_executor;
            let prediction = {
                let dfa_state_prediction = &mut self.get_dfa()
                    .states
                    .write().unwrap()
                    [state];
//                println!("accepted, prediction = {}, on dfastate {}", dfa_state_prediction.prediction, dfa_state_prediction.state_number);
//                lexer_action_executor = dfa_state_prediction.lexer_action_executor.clone();
//                let recog = self.recog.clone();
                dfa_state_prediction.lexer_action_executor.as_ref()
                    .map(|x| x.execute(lexer, self.start_index));

                dfa_state_prediction.prediction
            };
            self.accept(lexer.get_input_stream());
//            self.lexer_action_executor = lexer_action_executor;
            Ok(prediction)
        } else {
            if _t == EOF && lexer.get_input_stream().index() == self.start_index {
                return Ok(TOKEN_EOF);
            }
            Err(LexerNoAltError {
                start_index: self.start_index,
            })
        }
    }

    fn accept(&mut self, input: &mut dyn CharStream) {
        input.seek(self.prev_accept.index);
        self.current_pos.line.set(self.prev_accept.line);
        self.current_pos.char_position_in_line.set(self.prev_accept.column);
    }

    fn compute_start_state(&self, _p: &dyn ATNState, lexer: &mut dyn Lexer) -> Box<ATNConfigSet> {
        //        let initial_context = &EMPTY_PREDICTION_CONTEXT;
        let mut config_set = ATNConfigSet::new_ordered_atnconfig_set();

        for (i, tr) in _p.get_transitions().iter().enumerate() {
            let target = tr.get_target();
            let atn_config = ATNConfig::new_lexer_atnconfig6(
                target,
                (i + 1) as isize,
                EMPTY_PREDICTION_CONTEXT.clone(),
            );
            self.closure(
                atn_config,
                &mut config_set,
                false,
                false,
                false,
                lexer,
            );
        }

        Box::new(config_set)
    }

    fn closure(
        &self,
//        _input: &mut dyn CharStream,
        mut config: ATNConfig,
        _configs: &mut ATNConfigSet,
        mut _current_alt_reached_accept_state: bool,
        _speculative: bool,
        _treat_eofas_epsilon: bool,
        lexer: &mut dyn Lexer,
    ) -> bool {
        //        let config = &config;
        let atn = self.atn();
        let state = atn.states[config.get_state()].as_ref();
//        println!("closure called on state {} {:?}", state.get_state_number(), state.get_state_type());

        if let ATNStateType::RuleStopState {} = state.get_state_type() {
//            println!("reached rulestopstate {}",state.get_state_number());
            if config.get_context().map(|x| x.has_empty_path()) != Some(false) {
                if config.get_context().map(|x| x.is_empty()) != Some(false) {
                    _configs.add(Box::new(config));
                    return true;
                } else {
                    _configs.add(Box::new(config.cloned_with_new_ctx(
                        state,
                        Some(EMPTY_PREDICTION_CONTEXT.clone()),
                    )));
                    _current_alt_reached_accept_state = true
                }
            }

            if config.get_context().map(|x| x.is_empty()) == Some(false) {
                let mut ctx = config.take_context();
                for i in 0..ctx.length() {
                    if ctx.get_return_state(i) != PREDICTION_CONTEXT_EMPTY_RETURN_STATE {
                        let new_ctx = ctx.take_parent(i);
                        let return_state = self.atn().states[ctx.get_return_state(i) as usize].as_ref();
                        let next_config = config.cloned_with_new_ctx(return_state, new_ctx);
                        _current_alt_reached_accept_state = self.closure(
                            next_config,
                            _configs,
                            _current_alt_reached_accept_state,
                            _speculative,
                            _treat_eofas_epsilon,
                            lexer,
                        )
                    }
                }
            }

            return _current_alt_reached_accept_state;
        }

        if !state.has_epsilon_only_transitions() {
            if let ATNConfigType::LexerATNConfig { passed_through_non_greedy_decision, .. } = config.config_type {
                if !_current_alt_reached_accept_state || !passed_through_non_greedy_decision {
                    _configs.add(Box::new(config.clone()));
                }
            }
        }

        let state = atn.states[config.get_state()].as_ref();

        for tr in state.get_transitions() {
            let c = self.get_epsilon_target(
                &mut config,
                tr.as_ref(),
                _configs,
                _speculative,
                _treat_eofas_epsilon,
                lexer,
            );

            if let Some(c) = c {
                _current_alt_reached_accept_state = self.closure(
                    c,
                    _configs,
                    _current_alt_reached_accept_state,
                    _speculative,
                    _treat_eofas_epsilon,
                    lexer,
                );
            }
        }

        _current_alt_reached_accept_state
    }


    fn get_epsilon_target(
        &self,
//        _input: &mut dyn CharStream,
        _config: &mut ATNConfig,
        _trans: &dyn Transition,
        _configs: &mut ATNConfigSet,
        _speculative: bool,
        _treat_eofas_epsilon: bool,
        lexer: &mut dyn Lexer,
    ) -> Option<ATNConfig> {
        let mut result = None;
        let target = self.atn().states.get(_trans.get_target()).unwrap().as_ref();
//        println!("epsilon target for {:?} is {:?}", _trans, target.get_state_type());
        match _trans.get_serialization_type() {
            TransitionType::TRANSITION_EPSILON => {
                result = Some(_config.cloned(target));
            }
            TransitionType::TRANSITION_RULE => {
                let rt = _trans.cast::<RuleTransition>();
                //println!("rule transition follow state{}", rt.follow_state);
                let pred_ctx = PredictionContext::new_singleton(
                    Some(Box::new(_config.take_context())),
                    rt.follow_state as isize,
                );
                result = Some(_config.cloned_with_new_ctx(target, Some(pred_ctx)));
            }
            TransitionType::TRANSITION_PREDICATE => {
                let tr = _trans.cast::<PredicateTransition>();
                _configs.set_has_semantic_context(true);
                if self.evaluate_predicate(tr.rule_index, tr.pred_index, _speculative, lexer) {
                    result = Some(_config.cloned(target));
                }
            }
            TransitionType::TRANSITION_ACTION => {
                //println!("action transition");
                if _config.get_context().map(|x| x.has_empty_path()) != Some(false) {
                    if let ATNConfigType::LexerATNConfig { lexer_action_executor, .. } = _config.get_type() {
                        let tr = _trans.cast::<ActionTransition>();
                        let lexer_action = self.atn().lexer_actions[tr.action_index as usize].clone();
                        //dbg!(&lexer_action);
                        let lexer_action_executor = LexerActionExecutor::new_copy_append(lexer_action_executor.as_deref(), lexer_action);
                        result = Some(_config.cloned_with_new_exec(target, Some(lexer_action_executor)))
                    }
                } else {
                    result = Some(_config.cloned(target));
                }
            }
            TransitionType::TRANSITION_RANGE |
            TransitionType::TRANSITION_SET |
            TransitionType::TRANSITION_ATOM =>
                if _treat_eofas_epsilon {
                    if _trans.matches(EOF, LEXER_MIN_CHAR_VALUE, LEXER_MAX_CHAR_VALUE) {
                        let target = self.atn().states[_trans.get_target()].as_ref();
                        result = Some(_config.cloned(target));
                    }
                },
            TransitionType::TRANSITION_WILDCARD => {}
            TransitionType::TRANSITION_NOTSET => {}
            TransitionType::TRANSITION_PRECEDENCE => {
                panic!("precedence predicates are not supposed to be in lexer");
            }
        }

        result
    }

    fn evaluate_predicate(
        &self,
//        input: &mut dyn CharStream,
        rule_index: isize,
        pred_index: isize,
        speculative: bool,
        lexer: &mut dyn Lexer,
    ) -> bool {
        if !speculative {
            return lexer.sempred(&*empty_ctx(), rule_index, pred_index);
        }

        let saved_column = self.current_pos.char_position_in_line.get();
        let saved_line = self.current_pos.line.get();
        let index = lexer.get_input_stream().index();
        let marker = lexer.get_input_stream().mark();
        self.consume(lexer.get_input_stream());

        let result = lexer.sempred(&*empty_ctx(), rule_index, pred_index);

        self.current_pos.char_position_in_line.set(saved_column);
        self.current_pos.line.set(saved_line);
        lexer.get_input_stream().seek(index);
        lexer.get_input_stream().release(marker);
        return result;
    }

    fn capture_sim_state(&mut self, input: &dyn CharStream, dfa_state: DFAStateRef) -> bool {
        if self.get_dfa()
            .states
            .read().unwrap()
            .get(dfa_state).unwrap()
            .is_accept_state
        {
            self.prev_accept = SimState {
                index: input.index(),
                line: self.current_pos.line.get(),
                column: self.current_pos.char_position_in_line.get(),
                dfa_state: Some(dfa_state),
            };
            return true;
        }
        false
    }

    fn add_dfaedge(&self, _from: &mut DFAState, t: isize, _to: DFAStateRef) {
        if t < MIN_DFA_EDGE || t > MAX_DFA_EDGE {
            return;
        }

        if _from.edges.len() < (MAX_DFA_EDGE - MIN_DFA_EDGE + 1) as usize {
            _from.edges
                .resize((MAX_DFA_EDGE - MIN_DFA_EDGE + 1) as usize, 0);
        }
        _from.edges[(t - MIN_DFA_EDGE) as usize] = _to;
    }

    fn add_dfastate<V>(&self, states: &mut V, _configs: Box<ATNConfigSet>) -> DFAStateRef
        where
            V: DerefMut<Target=Vec<DFAState>>,
    {
        assert!(!_configs.has_semantic_context());
        let mut dfastate = DFAState::new_dfastate(usize::MAX, _configs);
        let rule_index = dfastate.configs//_configs
            .get_items()
            .find(|c|
                RuleStopState == *self.atn().states[c.get_state()].get_state_type()
            ).map(|c| {
            let rule_index = self.atn().states[c.get_state()].get_rule_index();

            //println!("accepted rule {} on state {}",rule_index,c.get_state());
            (self.atn().rule_to_token_type[rule_index],
             c.get_lexer_executor().map(LexerActionExecutor::clone).map(Box::new))
        });

        if let Some((prediction, exec)) = rule_index {
            dfastate.prediction = prediction;
            dfastate.lexer_action_executor = exec;
            dfastate.is_accept_state = true;
        }

        let dfa = self.get_dfa();
        let key = dfastate.default_hash();
        let dfastate_index = *dfa.states_map.write().unwrap()
            .entry(key)
            .or_insert_with(|| {
                dfastate.state_number = states.deref().len();
                dfastate.configs.set_read_only(true);
                let i = dfastate.state_number;
                //println!("inserting new DFA state {} with size {}", i, dfastate.configs.length());
                states.deref_mut().push(dfastate);
                vec![i]
            })
            .first().unwrap();

        //println!("new DFA state {}", dfastate_index);

        //        dfa.states.write().unwrap().get_mut(*dfastate_index).unwrap()
        dfastate_index
    }

    pub fn get_dfa(&self) -> &DFA {
        &self.decision_to_dfa()[self.mode]
    }

    pub fn get_dfa_for_mode(&self, mode: usize) -> &DFA {
        &self.decision_to_dfa()[mode]
    }

    fn get_token_name(&self, _tt: isize) -> String {
        unimplemented!()
    }

    fn reset_sim_state(_sim: &mut SimState) {
        unimplemented!()
    }
}

pub struct SimState {
    index: isize,
    line: isize,
    column: isize,
    dfa_state: Option<usize>,
}

impl SimState {
    pub fn new() -> SimState {
        SimState {
            index: -1,
            line: 0,
            column: -1,
            dfa_state: None,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.index = -1;
        self.line = 0;
        self.column = -1;
        self.dfa_state = None;
    }
}
