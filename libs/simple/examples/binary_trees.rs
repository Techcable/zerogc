use zerogc::{safepoint, safepoint_recurse, GcSimpleAlloc, GcCell, Trace, GcVisitor, GcBrand, GcSafe, GcSystem};

use zerogc_simple::{SimpleCollector, SimpleCollectorContext, Gc};

struct Tree<'gc> {
    children: GcCell<Option<(Gc<'gc, Tree<'gc>>, Gc<'gc, Tree<'gc>>)>>,
}
// TODO: Auto-derive
unsafe impl<'gc> Trace for Tree<'gc> {
    const NEEDS_TRACE: bool = true;

    #[inline]
    fn visit<V: GcVisitor>(&mut self, visitor: &mut V) -> Result<(), V::Err> {
        visitor.visit(&mut self.children)
    }
}
unsafe impl<'gc, 'new_gc, S: GcSystem> GcBrand<'new_gc, S> for Tree<'gc> {
    type Branded = Tree<'new_gc>;
}
unsafe impl<'gc> GcSafe for Tree<'gc> {}

fn item_check(tree: &Tree) -> i32 {
    if let Some((left, right)) = tree.children.get() {
        1 + item_check(&right) + item_check(&left)
    } else {
        1
    }
}

fn bottom_up_tree<'gc>(collector: &'gc SimpleCollectorContext, depth: i32)
                       -> Gc<'gc, Tree<'gc>> {
    let tree = collector.alloc(Tree { children: GcCell::new(None) });
    if depth > 0 {
        let right = bottom_up_tree(collector, depth - 1);
        let left = bottom_up_tree(collector, depth - 1);
        tree.children.set(Some((left, right)));
    }
    tree
}

fn inner(
    gc: &mut SimpleCollectorContext,
    depth: i32, iterations: u32
) -> String {
    let chk: i32 = (0 .. iterations).into_iter().map(|_| {
        safepoint_recurse!(gc, |gc, new_root| {
            let () = new_root;
            let a = bottom_up_tree(&gc, depth);
            item_check(&a)
        })
    }).sum();
    format!("{}\t trees of depth {}\t check: {}", iterations, depth, chk)
}

fn main() {
    let n = std::env::args().nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(10);
    let min_depth = 4;
    let max_depth = if min_depth + 2 > n { min_depth + 2 } else { n };

    let collector = SimpleCollector::create();
    let mut gc = collector.into_context();
    {
        let depth = max_depth + 1;
        let tree = bottom_up_tree(&gc, depth);
        println!("stretch tree of depth {}\t check: {}", depth, item_check(&tree));
    }
    safepoint!(gc, ());

    let long_lived_tree = bottom_up_tree(&gc, max_depth);

    let (long_lived_tree, ()) = safepoint_recurse!(gc, long_lived_tree, |gc, long_lived_tree| {
        (min_depth / 2..max_depth / 2 + 1).into_iter().for_each(|half_depth| {
            let depth = half_depth * 2;
            let iterations = 1 << ((max_depth - depth + min_depth) as u32);
            let message = safepoint_recurse!(gc, |new_gc, new_root| {
                inner(&mut new_gc, depth, iterations)
            });
            println!("{}", message);
        })
    });

    println!("long lived tree of depth {}\t check: {}", max_depth, item_check(&long_lived_tree));
}