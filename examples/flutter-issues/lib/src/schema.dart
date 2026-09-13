// The application's schema, and the only file in the app that knows it.
//
// Plan §4.1's byte ABI means no Rust was regenerated to run this app: the FFI
// signatures are `Vec<u8>` in and `Vec<u8>` out, and everything about issues
// and comments lives on this side. In M1 this file is what `solstice_builder`
// emits from a schema declaration. In M0 it is written by hand, which is the
// honest way to prove the property — if a hand-written consumer can drive the
// engine without touching the crate, a generated one certainly can.

import 'dart:typed_data';

import 'package:fixnum/fixnum.dart';

import '../gen/solstice/v1/mutation.pb.dart' as mut;
import '../gen/solstice/v1/query.pb.dart' as q;
import '../gen/solstice/v1/view.pb.dart';

/// Column positions, matching `solstice_core::m0`.
///
/// Rows on the wire are positional (`view.proto`: "column *i* of the row is the
/// column the schema declares at index *i*"), so these constants are the schema
/// as far as the app is concerned. Names appear only where a mutation has to
/// name a column it is setting.
abstract final class Issue {
  static const id = 0;
  static const projectId = 1;
  static const priority = 2;
  static const closed = 3;
  static const title = 4;
  static const updatedAt = 5;
  static const columns = 6;

  static const table = 'issues';
  static const colPriority = 'priority';
  static const colUpdatedAt = 'updated_at';
}

abstract final class Comment {
  static const id = 0;
  static const issueId = 1;
  static const createdAt = 2;
  static const author = 3;
  static const body = 4;
  static const columns = 5;

  static const table = 'comments';
}

/// The query from plan §7, which is the one the whole design is at risk on:
/// "the top 50 issues by priority whose assignee is on my team, each with its
/// three latest comments". Joined, sorted, limited — the combination §7 says
/// degrades into unbounded requery under deletes if it is done wrong.
///
/// `project` is a bound parameter rather than a literal. Plan §1.2 hashes the
/// IR twice, once without parameters for the `PipelineId` and once with them
/// for the `ViewId`, so that N users running this query for N projects share
/// one dataflow pipeline on the server. Folding the project in as a constant
/// would encode a query shape the real system never runs.
Uint8List buildQuery({
  required int project,
  required int k,
  required int comments,
}) {
  final query = q.Query()
    ..table = Issue.table
    ..where = (q.Predicate()
      ..and = (q.PredicateList()
        ..preds.addAll([
          q.Predicate()
            ..cmp = (q.Cmp()
              ..lhs = (q.Expr()..col = 'project_id')
              ..op = q.CmpOp.CMP_OP_EQ
              ..rhs = (q.Expr()..param = 0)),
          q.Predicate()
            ..cmp = (q.Cmp()
              ..lhs = (q.Expr()..col = 'closed')
              ..op = q.CmpOp.CMP_OP_EQ
              ..rhs = (q.Expr()..lit = (Value()..integer = Int64.ZERO))),
        ])))
    ..orderBy.add(q.Order()
      ..col = Issue.colPriority
      ..desc = true)
    ..limit = k
    // Through the factory rather than a cascade: `as` is a Dart keyword, so
    // `..as = 'comments'` parses as a cast and will not compile.
    ..related.add(q.Related(
      relName: 'comments',
      as: 'comments',
      sub: q.Query()
        ..table = Comment.table
        ..orderBy.add(q.Order()
          ..col = 'created_at'
          ..desc = true)
        ..limit = comments,
    ))
    ..params.add(Value()..integer = Int64(project));
  return query.writeToBuffer();
}

/// A transaction, built from ops the driver produced.
///
/// One `Mutation` carries a whole `Patch`, and the engine turns it into one
/// graph pump at one version (plan §1.4). That matters for what this app
/// measures: a transaction touching issues *and* comments must reach the view
/// as a single delta, or the list would show a torn read halfway through.
Uint8List buildMutation({required int mutationId, required List<mut.Op> ops}) {
  final m = mut.Mutation()
    ..clientId = Uint8List.fromList([1])
    ..mutationId = Int64(mutationId)
    ..patch = (mut.Patch()..ops.addAll(ops));
  return m.writeToBuffer();
}

mut.Op updateIssue(int id, Map<String, int> set) => mut.Op()
  ..update = (mut.Update()
    ..table = Issue.table
    ..key = (Value()..integer = Int64(id))
    ..set.addAll(set.entries.map((e) => mut.ColValue()
      ..col = e.key
      ..value = (Value()..integer = Int64(e.value)))));

mut.Op insertIssue({
  required int id,
  required int projectId,
  required int priority,
  required bool closed,
  required String title,
  required int updatedAt,
}) =>
    mut.Op()
      ..insert = (mut.Insert()
        ..table = Issue.table
        ..row = (Row()
          ..values.addAll([
            Value()..integer = Int64(id),
            Value()..integer = Int64(projectId),
            Value()..integer = Int64(priority),
            Value()..integer = Int64(closed ? 1 : 0),
            Value()..text = title,
            Value()..integer = Int64(updatedAt),
          ])));

mut.Op insertComment({
  required int id,
  required int issueId,
  required int createdAt,
  required String author,
  required String body,
}) =>
    mut.Op()
      ..insert = (mut.Insert()
        ..table = Comment.table
        ..row = (Row()
          ..values.addAll([
            Value()..integer = Int64(id),
            Value()..integer = Int64(issueId),
            Value()..integer = Int64(createdAt),
            Value()..text = author,
            Value()..text = body,
          ])));

mut.Op deleteComment(int id) => mut.Op()
  ..delete = (mut.Delete()
    ..table = Comment.table
    ..key = (Value()..integer = Int64(id)));
