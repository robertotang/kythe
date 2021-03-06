// Copyright 2016 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use kythe::corpus::Corpus;
use kythe::schema::{VName, Fact, NodeKind, EdgeKind, Complete};
use kythe::writer::EntryWriter;
use rustc::lint::{LateContext, LintContext, LintPass, LateLintPass, LintArray};
use rustc::hir;
use rustc::hir::def_id::DefId;
use std::collections::HashSet;
use std::io::stderr;
use std::io::prelude::*;
use syntax::ast;
use syntax::codemap::{Pos, CodeMap, Span};

// Represents the shared data between all the check_* functions in rustc's LateLintPass
pub struct KytheLintPass {
    pub corpus: Corpus,
    pub writer: Box<EntryWriter>,
    // Some definitions are uninteresting such as those from for-loop desugaring.
    // This hashset tracks their ID's so they can be ignored.
    blacklist: HashSet<DefId>,
}

impl KytheLintPass {
    pub fn new(corpus: Corpus, writer: Box<EntryWriter>) -> KytheLintPass {
        KytheLintPass {
            corpus: corpus,
            writer: writer,
            blacklist: HashSet::new(),
        }
    }
    // Emits the appropriate node and facts for an anchor defined by a span
    // and returns the node's vname
    fn anchor_from_span(&self, span: Span, codemap: &CodeMap) -> VName {
        let start = codemap.lookup_byte_offset(span.lo);
        let end = codemap.lookup_byte_offset(span.hi);

        let start_byte = start.pos.to_usize();
        let end_byte = end.pos.to_usize();
        self.anchor(&start.fm.name, start_byte, end_byte)
    }
    // Emits the appropriate node and facts for an anchor defined as a substring within a span
    // and returns the node's VName.
    fn anchor_from_sub_span(&self,
                            span: Span,
                            sub: &str,
                            codemap: &CodeMap)
                            -> Result<VName, String> {
        let start = codemap.lookup_byte_offset(span.lo);
        let snippet = match codemap.span_to_snippet(span) {
            Ok(s) => s,
            Err(e) => return Err(format!("{:?}", e)),
        };
        let sub_start = match snippet.find(sub) {
            None => return Err(format!("Substring: '{}' not found in snippet '{}'", sub, snippet)),
            Some(s) => s,
        };
        let start_byte = start.pos.to_usize() + sub_start;
        let end_byte = start_byte + sub.len();

        Ok(self.anchor(&start.fm.name, start_byte, end_byte))
    }

    fn anchor(&self, file_name: &str, start_byte: usize, end_byte: usize) -> VName {
        let vname = self.corpus.anchor_vname(file_name, start_byte, end_byte);

        let start_str: String = start_byte.to_string();
        let end_str: String = end_byte.to_string();
        let file_vname = self.corpus.file_vname(&file_name);

        self.writer.node(&vname, Fact::NodeKind, &NodeKind::Anchor);
        self.writer.node(&vname, Fact::LocStart, &start_str);
        self.writer.node(&vname, Fact::LocEnd, &end_str);
        self.writer.edge(&vname, EdgeKind::ChildOf, &file_vname);
        vname
    }

    fn vname_from_defid(&self, cx: &LateContext, def_id: DefId) -> VName {
        let def_id_num = def_id.index.as_u32();
        let var_name = cx.tcx.absolute_item_path_str(def_id);
        self.corpus.def_vname(&var_name, def_id_num)
    }

    // Emits the appropriate ref/call and childof nodes for a function call.
    fn function_call(&self, cx: &LateContext, call_node_id: ast::NodeId, callee_def_id: DefId) {
        let ref map = cx.tcx.map;

        let call_span = map.span(call_node_id);
        // The call anchor includes the subject (if function is a method) and the params
        let call_anchor_vname = self.anchor_from_span(call_span, cx.sess().codemap());
        let callee_vname = self.vname_from_defid(cx, callee_def_id);

        self.writer.edge(&call_anchor_vname, EdgeKind::RefCall, &callee_vname);

        let parent_did = map.get_parent_did(call_node_id);
        let parent_node_id = map.get_parent(call_node_id);

        // Although we could just attach a childof edge without inspecting the parent,
        // this check prevents unintended childof edges should the call have an unexpected parent
        use rustc::hir::map::Node::{NodeItem, NodeImplItem};
        use rustc::hir::Item_::ItemFn;
        use rustc::hir::ImplItemKind::Method;
        if let Some(parent_node) = map.find(parent_node_id) {
            match parent_node {
                // The parent can be either a function or method
                NodeItem(&hir::Item { node: ItemFn(..), .. }) |
                NodeImplItem(&hir::ImplItem { node: Method(..), .. }) => {
                    let parent_vname = self.vname_from_defid(cx, parent_did);
                    self.writer.edge(&call_anchor_vname, EdgeKind::ChildOf, &parent_vname);
                }
                _ => (),
            }
        }
    }
}

// A no-op implementation since we aren't reporting linting errors
impl LintPass for KytheLintPass {
    fn get_lints(&self) -> LintArray {
        lint_array!()
    }
}

impl LateLintPass for KytheLintPass {
    fn check_crate_post(&mut self, cx: &LateContext, _: &hir::Crate) {
        for (&ref_node, def) in cx.tcx.def_map.borrow().iter() {
            use rustc::hir::def::Def;
            match def.base_def {
                // Capture function & direct method reference (methods that don't use &self)
                Def::Method(def_id) |
                Def::Fn(def_id) => {
                    let ref_span = cx.tcx.map.span(ref_node);
                    let is_from_macro = ref_span.expn_id.into_u32() != u32::max_value();
                    if is_from_macro {
                        continue;
                    }

                    let local_vname = self.vname_from_defid(cx, def_id);

                    // We want to determine if this reference is part of a call
                    // so we need to lookup the expression and check
                    use rustc::hir::map::Node::NodeExpr;
                    use rustc::hir::Expr_::{ExprCall, ExprMatch};
                    use rustc::hir::MatchSource::ForLoopDesugar;

                    // The reference will always be to the path name for the function which isn't
                    // helpful in determining what kind of reference so we look up one level
                    let possible_call_node = cx.tcx.map.find(cx.tcx.map.get_parent_node(ref_node));

                    // It could only be a function call if the reference were contained in an
                    // expression
                    if let Some(NodeExpr(expr)) = possible_call_node {
                        if let ExprCall(_, _) = expr.node {
                            // Unfortunately, for loop desugaring adds a function call for the
                            // iterator that we want to handle differently. Otherwise, the full
                            // text of the for loop will be claimed as a function call
                            let parent = cx.tcx.map.find(cx.tcx.map.get_parent_node(expr.id));

                            // If the parent expression to the function call is a match statement
                            // that is marked as a ForLoop Desugar we will ignore it.
                            if let Some(NodeExpr(p_expr)) = parent {
                                if let ExprMatch(_, _, ForLoopDesugar) = p_expr.node {
                                    continue;
                                }
                            }
                            self.function_call(cx, expr.id, def_id);
                        }
                    }

                    // Emit a ref since we've established we aren't in a for loop desugaring.
                    let ref_anchor_vname = self.anchor_from_span(ref_span, cx.sess().codemap());
                    self.writer.edge(&ref_anchor_vname, EdgeKind::Ref, &local_vname);
                }

                Def::Local(def_id, def_node) => {
                    let ref_span = cx.tcx.map.span(ref_node);
                    let is_from_macro = ref_span.expn_id.into_u32() != u32::max_value();

                    // Skip the definition since we've marked it as uninteresting
                    if is_from_macro || self.blacklist.contains(&def_id) {
                        continue;
                    }

                    let local_vname = self.vname_from_defid(cx, def_id);
                    let anchor_vname = self.anchor_from_span(ref_span, cx.sess().codemap());

                    // If the ref_node is the def_node we emit defines/binding not ref.
                    if ref_node == def_node {
                        self.writer.node(&local_vname, Fact::NodeKind, &NodeKind::Variable);
                        self.writer.edge(&anchor_vname, EdgeKind::DefinesBinding, &local_vname);
                    } else {
                        self.writer.edge(&anchor_vname, EdgeKind::Ref, &local_vname);
                    }
                }
                _ => (),
            }
        }
    }

    fn check_crate(&mut self, cx: &LateContext, _: &hir::Crate) {
        let codemap = cx.sess().codemap();

        // The method map contains a mapping from instance method calls to their definition.
        // This map does not include static methods.
        for (call, callee) in cx.tcx.tables.borrow().method_map.iter() {
            let call_expr = cx.tcx.map.expect_expr(call.expr_id);
            if let hir::Expr_::ExprMethodCall(sp_name, _, _) = call_expr.node {
                let ref_span = sp_name.span;
                let is_from_macro = ref_span.expn_id.into_u32() != u32::max_value();
                if is_from_macro {
                    continue;
                }

                let local_vname = self.vname_from_defid(cx, callee.def_id);

                // The ref anchor only includes the method name
                let ref_anchor_vname = self.anchor_from_span(sp_name.span, codemap);

                self.function_call(cx, call.expr_id, callee.def_id);
                self.writer.edge(&ref_anchor_vname, EdgeKind::Ref, &local_vname);
            }
        }

        for ref f in codemap.files.borrow().iter() {
            // The codemap contains references to virtual files all labeled <<std_macro>>
            // These are skipped as per this check
            if !f.is_real_file() {
                continue;
            }

            // References to the core crate are filtered out here
            if let Some(ref content) = f.src {
                let vname = self.corpus.file_vname(&f.name);

                self.writer.node(&vname, Fact::NodeKind, &NodeKind::File);
                self.writer.node(&vname, Fact::Text, content);
            }
        }
    }

    fn check_local(&mut self, cx: &LateContext, local: &hir::Local) {
        // For loops desugar into a let and a match statement that contain local
        // variables that we don't want to expose. The only way to
        // tell that this is happening is at the match expression level.
        // We want to find these variables and add them to the blacklist.

        // If the local has no init value, it's safe to report
        if let Some(ref init) = local.init {
            use rustc::hir::Expr_::ExprMatch;
            // Verify that the init statement comes from a Desugar
            if let ExprMatch(_, ref arms, hir::MatchSource::ForLoopDesugar) = init.node {

                // Blacklist the lefthand side of the let statement
                if let Some(def_id) = cx.tcx.map.opt_local_def_id(local.pat.id) {
                    self.blacklist.insert(def_id);
                }

                // In the event that the exact representation of the desugared for loop
                // changes, we may as well invalidate all branches
                for arm in arms {
                    for pat in &arm.pats {
                        // Then we grab the def_id and insert into the blacklist
                        if let Some(def_id) = cx.tcx.map.opt_local_def_id(pat.id) {
                            self.blacklist.insert(def_id);
                        }
                    }
                }
            }
        }
    }

    fn check_fn_post(&mut self,
                     cx: &LateContext,
                     kind: hir::intravisit::FnKind,
                     _: &hir::FnDecl,
                     block: &hir::Block,
                     span: Span,
                     _: ast::NodeId) {
        match kind {

            // Create the definition nodes and edges for functions and methods
            hir::intravisit::FnKind::ItemFn(n, _, _, _, _, _, _) |
            hir::intravisit::FnKind::Method(n, _, _, _) => {

                let fn_name = n.to_string();
                let def_id = cx.tcx.map.get_parent_did(block.id);
                let fn_vname = self.vname_from_defid(cx, def_id);
                let decl_vname = self.anchor_from_span(span, cx.sess().codemap());

                self.writer.node(&fn_vname, Fact::NodeKind, &NodeKind::Function);
                self.writer.node(&fn_vname, Fact::Complete, &Complete::Definition);
                self.writer.edge(&decl_vname, EdgeKind::Defines, &fn_vname);

                match self.anchor_from_sub_span(span, &fn_name, cx.sess().codemap()) {
                    Ok(bind_vname) => {
                        self.writer.edge(&bind_vname, EdgeKind::DefinesBinding, &fn_vname)
                    }

                    Err(_) => self.writer.edge(&decl_vname, EdgeKind::DefinesBinding, &fn_vname),
                }
            }
            _ => (),
        }
    }
}
