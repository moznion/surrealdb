use crate::idx::docids::DocId;
use crate::idx::trees::knn::{Docs, KnnResult, KnnResultBuilder, PriorityNode};
use crate::idx::trees::vector::SharedVector;
use crate::sql::index::Distance;
use rand::prelude::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use tokio::sync::RwLock;

struct HnswIndex<const M: usize, const M0: usize, const EFC: usize> {
	h: Hnsw<M, M0, EFC>,
	d: HashMap<SharedVector, Docs>,
}

impl<const M: usize, const M0: usize, const EFC: usize> HnswIndex<M, M0, EFC> {
	fn new(distance: Distance) -> Self {
		let h = Hnsw::new(None, distance);
		let d = HashMap::new();
		HnswIndex {
			h,
			d,
		}
	}

	async fn insert(&mut self, o: SharedVector, d: DocId) {
		self.h.insert(o.clone()).await;
		match self.d.entry(o) {
			Entry::Occupied(mut e) => {
				let docs = e.get_mut();
				if let Some(new_docs) = docs.insert(d) {
					e.insert(new_docs);
				}
			}
			Entry::Vacant(e) => {
				e.insert(Docs::One(d));
			}
		}
	}

	async fn search(&mut self, o: &SharedVector, n: usize, ef: usize) -> KnnResult {
		let neighbors = self.h.knn_search(o, n, ef).await;
		let mut builder = KnnResultBuilder::new(n);
		for pn in neighbors {
			if builder.check_add(pn.0) {
				let v = &self.h.elements[pn.1 as usize];
				if let Some(docs) = self.d.get(v) {
					builder.add(pn.0, docs);
				}
			}
		}

		builder.build(
			#[cfg(debug_assertions)]
			HashMap::new(),
		)
	}
}

struct Hnsw<const M: usize, const M0: usize, const EFC: usize> {
	ml: f64,
	dist: Distance,
	layers: Vec<RwLock<Layer>>,
	enter_point: Option<ElementId>,
	elements: Vec<SharedVector>,
	rng: SmallRng,
}

struct Layer(HashMap<ElementId, Vec<ElementId>>);

impl Layer {
	fn new() -> Self {
		Self(HashMap::with_capacity(1))
	}
}

type ElementId = u64;

impl<const M: usize, const M0: usize, const EFC: usize> Hnsw<M, M0, EFC> {
	fn new(ml: Option<f64>, dist: Distance) -> Self {
		debug!("NEW - M0: {M0} - M: {M} - ml: {ml:?}");
		Self {
			ml: ml.unwrap_or(1.0 / (M as f64).ln()),
			dist,
			enter_point: None,
			layers: Vec::default(),
			elements: Vec::default(),
			rng: SmallRng::from_entropy(),
		}
	}

	async fn insert(&mut self, q: SharedVector) -> ElementId {
		let id = self.elements.len() as ElementId;
		let level = self.get_random_level();
		let layers = self.layers.len();

		for l in layers..=level {
			debug!("Create Layer {l}");
			self.layers.push(RwLock::new(Layer::new()));
		}

		if let Some(ep) = self.enter_point {
			self.insert_element(&q, ep, id, level, layers - 1).await;
		} else {
			self.insert_first_element(id, level).await;
		}

		self.elements.push(q);
		id
	}

	fn get_random_level(&mut self) -> usize {
		let unif: f64 = self.rng.gen(); // generate a uniform random number between 0 and 1
		(-unif.ln() * self.ml).floor() as usize // calculate the layer
	}

	async fn insert_first_element(&mut self, id: ElementId, level: usize) {
		debug!("insert_first_element - id: {id} - level: {level}");
		for lc in 0..=level {
			self.layers[lc].write().await.0.insert(id, vec![]);
		}
		self.enter_point = Some(id);
		debug!("E - EP: {id}");
	}

	async fn insert_element(
		&mut self,
		q: &SharedVector,
		mut ep: ElementId,
		id: ElementId,
		level: usize,
		top_layer_level: usize,
	) {
		debug!("insert_element q: {q:?} - id: {id} - level: {level} -  ep: {ep:?} - top-layer: {top_layer_level}");

		for lc in ((level + 1)..=top_layer_level).rev() {
			let w = self.search_layer(q, ep, 1, lc).await;
			if let Some(n) = w.first() {
				ep = n.1;
			}
		}

		// TODO: One thread per level
		let mut m_max = M;
		for lc in (0..=top_layer_level.min(level)).rev() {
			if lc == 0 {
				m_max = M0;
			}
			debug!("2 - LC: {lc}");
			let w = self.search_layer(q, ep, EFC, lc).await;
			debug!("2 - W: {w:?}");
			let mut neighbors = Vec::with_capacity(m_max.min(w.len()));
			self.select_neighbors_simple(&w, m_max, &mut neighbors);
			debug!("2 - N: {neighbors:?}");
			// add bidirectional connections from neighbors to q at layer lc
			let mut layer = self.layers[lc].write().await;
			layer.0.insert(id, neighbors.clone());
			debug!("2 - Layer: {:?}", layer.0);
			for e_id in neighbors {
				if let Some(e_conn) = layer.0.get_mut(&e_id) {
					if e_conn.len() >= m_max {
						self.select_and_shrink_neighbors_simple(e_id, id, q, e_conn, m_max);
					} else {
						e_conn.push(id);
					}
				} else {
					unreachable!("Element: {}", e_id)
				}
			}
			if let Some(n) = w.first() {
				ep = n.1;
				debug!("2 - EP: {ep}");
			} else {
				unreachable!("W is empty")
			}
		}

		for lc in (top_layer_level + 1)..=level {
			let mut layer = self.layers[lc].write().await;
			if layer.0.insert(id, vec![]).is_some() {
				unreachable!("Already there {id}");
			}
		}

		if level > top_layer_level {
			self.enter_point = Some(id);
			debug!("E - EP: {id}");
		}
		self.debug_print_check().await;
	}

	async fn debug_print_check(&self) {
		debug!("EP: {:?}", self.enter_point);
		for (i, l) in self.layers.iter().enumerate() {
			let l = l.read().await;
			debug!("LAYER {i} {:?}", l.0);
			let m_max = if i == 0 {
				M0
			} else {
				M
			};
			for f in l.0.values() {
				assert!(f.len() <= m_max);
			}
		}
	}

	/// query element q
	/// enter points ep
	/// number of nearest to q
	/// elements to return ef
	/// layer number lc
	/// Output: ef closest neighbors to q
	async fn search_layer(
		&self,
		q: &SharedVector,
		ep_id: ElementId,
		ef: usize,
		lc: usize,
	) -> BTreeSet<PriorityNode> {
		let ep_dist = self.distance(&self.elements[ep_id as usize], q);
		let ep_pr = PriorityNode(ep_dist, ep_id);
		let mut candidates = BTreeSet::from([ep_pr.clone()]);
		let mut w = BTreeSet::from([ep_pr]);
		let mut visited = HashSet::from([ep_id]);
		while let Some(c) = candidates.pop_first() {
			let f_dist = candidates.last().map(|f| f.0).unwrap_or(c.0);
			if c.0 > f_dist {
				break;
			}
			for (&e_id, e_neighbors) in &self.layers[lc].read().await.0 {
				if e_neighbors.contains(&c.1) {
					if visited.insert(e_id) {
						let e_dist = self.distance(&self.elements[e_id as usize], q);
						if e_dist < f_dist || w.len() < ef {
							candidates.insert(PriorityNode(e_dist, e_id));
							w.insert(PriorityNode(e_dist, e_id));
							if w.len() > ef {
								w.pop_last();
							}
						}
					}
				}
			}
		}
		w
	}

	fn select_and_shrink_neighbors_simple(
		&self,
		e_id: ElementId,
		new_f_id: ElementId,
		new_f: &SharedVector,
		elements: &mut Vec<ElementId>,
		m_max: usize,
	) {
		let e = &self.elements[e_id as usize];
		let mut w = BTreeSet::default();
		w.insert(PriorityNode(self.distance(e, new_f), new_f_id));
		for f_id in elements.drain(..) {
			let f_dist = self.distance(&self.elements[f_id as usize], e);
			w.insert(PriorityNode(f_dist, f_id));
		}
		self.select_neighbors_simple(&w, m_max, elements);
	}

	fn select_neighbors_simple(
		&self,
		w: &BTreeSet<PriorityNode>,
		m_max: usize,
		neighbors: &mut Vec<ElementId>,
	) {
		for pr in w {
			neighbors.push(pr.1);
			if neighbors.len() == m_max {
				break;
			}
		}
	}

	fn distance(&self, v1: &SharedVector, v2: &SharedVector) -> f64 {
		self.dist.dist(v1, v2)
	}

	async fn knn_search(&self, q: &SharedVector, k: usize, ef: usize) -> Vec<PriorityNode> {
		if let Some(mut ep) = self.enter_point {
			let l = self.layers.len();
			for lc in (1..l).rev() {
				let w = self.search_layer(q, ep, 1, lc).await;
				if let Some(n) = w.first() {
					ep = n.1;
				} else {
					unreachable!()
				}
			}
			let w = self.search_layer(q, ep, ef, 0).await;
			let w: Vec<PriorityNode> = w.into_iter().collect();
			w.into_iter().take(k).collect()
		} else {
			vec![]
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::err::Error;
	use crate::idx::docids::DocId;
	use crate::idx::trees::hnsw::HnswIndex;
	use crate::idx::trees::knn::tests::{get_seed_rnd, new_random_vec, TestCollection};
	use crate::idx::trees::vector::SharedVector;
	use crate::sql::index::{Distance, VectorType};
	use std::collections::HashMap;
	use test_log::test;

	async fn insert_collection_one_by_one<const M: usize, const M0: usize, const EFC: usize>(
		h: &mut HnswIndex<M, M0, EFC>,
		collection: &TestCollection,
	) -> Result<HashMap<DocId, SharedVector>, Error> {
		let mut map = HashMap::with_capacity(collection.as_ref().len());
		for (doc_id, obj) in collection.as_ref() {
			h.insert(obj.clone(), *doc_id).await;
			map.insert(*doc_id, obj.clone());
		}
		Ok(map)
	}

	async fn find_collection<const M: usize, const M0: usize, const EFC: usize>(
		h: &mut HnswIndex<M, M0, EFC>,
		collection: &TestCollection,
	) -> Result<(), Error> {
		let max_knn = 20.max(collection.as_ref().len());
		for (doc_id, obj) in collection.as_ref() {
			for knn in 1..max_knn {
				let res = h.search(obj, knn, 500).await;
				let docs: Vec<DocId> = res.docs.iter().map(|(d, _)| *d).collect();
				if collection.is_unique() {
					assert!(
						docs.contains(doc_id),
						"Search: {:?} - Knn: {} - Wrong Doc - Expected: {} - Got: {:?} - Dist: {} - Coll: {:?}",
						obj,
						knn,
						doc_id,
						res.docs,
						h.h.dist,
						collection,
					);
				}
				let expected_len = collection.as_ref().len().min(knn);
				assert_eq!(
					expected_len,
					res.docs.len(),
					"Wrong knn count - Expected: {} - Got: {:?} - Dist: {} - Collection: {}",
					expected_len,
					res.docs,
					h.h.dist,
					collection.as_ref().len(),
				)
			}
		}
		Ok(())
	}

	async fn test_hnsw_collection<const M: usize, const M0: usize, const EFC: usize>(
		distance: Distance,
		collection: &TestCollection,
	) -> Result<(), Error> {
		let mut h: HnswIndex<M, M0, EFC> = HnswIndex::new(distance);
		insert_collection_one_by_one::<M, M0, EFC>(&mut h, collection).await?;
		find_collection::<M, M0, EFC>(&mut h, &collection).await?;
		Ok(())
	}

	#[test(tokio::test)]
	async fn test_hnsw_unique_col_10_dim_2() -> Result<(), Error> {
		for vt in
			[VectorType::F64, VectorType::F32, VectorType::I64, VectorType::I32, VectorType::I16]
		{
			for distance in [
				Distance::Euclidean,
				Distance::Manhattan,
				Distance::Hamming,
				Distance::Minkowski(2.into()),
				Distance::Chebyshev,
			] {
				let for_jaccard = distance == Distance::Jaccard;
				test_hnsw_collection::<12, 24, 500>(
					distance,
					&TestCollection::new_unique(10, vt, 2, for_jaccard),
				)
				.await?;
			}
		}
		Ok(())
	}

	#[test(tokio::test)]
	async fn test_hnsw_random_col_10_dim_2() -> Result<(), Error> {
		for vt in
			[VectorType::F64, VectorType::F32, VectorType::I64, VectorType::I32, VectorType::I16]
		{
			for distance in [
				// Distance::Chebyshev, TODO
				Distance::Cosine,
				Distance::Euclidean,
				// Distance::Hamming, TODO
				// Distance::Jaccard, TODO
				Distance::Manhattan,
				Distance::Minkowski(2.into()),
				// Distance::Pearson,  TODO
			] {
				let for_jaccard = distance == Distance::Jaccard;
				test_hnsw_collection::<12, 24, 500>(
					distance,
					&TestCollection::new_random(10, vt, 2, for_jaccard),
				)
				.await?;
			}
		}
		Ok(())
	}

	#[test(tokio::test)]
	async fn test_hnsw_unique_coll_20_dim_1536() -> Result<(), Error> {
		for vt in [VectorType::F32, VectorType::I32] {
			test_hnsw_collection::<12, 24, 500>(
				Distance::Hamming,
				&TestCollection::new_unique(20, vt, 1536, false),
			)
			.await?;
		}
		Ok(())
	}

	fn test_distance(dist: Distance, size: usize, dim: usize) {
		let mut rng = get_seed_rnd();
		let mut coll = Vec::with_capacity(size);
		for vt in
			[VectorType::F64, VectorType::F32, VectorType::I64, VectorType::I32, VectorType::I16]
		{
			let integer = dist == Distance::Jaccard;
			for _ in 0..size {
				let v1 = new_random_vec(&mut rng, vt, dim, integer);
				let v2 = new_random_vec(&mut rng, vt, dim, integer);
				coll.push((v1, v2));
			}
			let mut num_zero = 0;
			for (i, (v1, v2)) in coll.iter().enumerate() {
				let d = dist.dist(v1, v2);
				assert!(
					d.is_finite() && !d.is_nan(),
					"i: {i} - vt: {vt} - v1: {v1:?} - v2: {v2:?}"
				);
				assert_ne!(d, f64::NAN, "i: {i} - vt: {vt} - v1: {v1:?} - v2: {v2:?}");
				assert_ne!(d, f64::INFINITY, "i: {i} - vt: {vt} - v1: {v1:?} - v2: {v2:?}");
				if d == 0.0 {
					num_zero += 1;
				}
			}
			let zero_rate = num_zero as f64 / size as f64;
			assert!(zero_rate < 0.1, "vt: {vt} - zero_rate: {zero_rate}");
		}
	}

	#[test]
	fn test_distance_chebyshev() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Chebyshev);
		test_distance(h.h.dist, 2000, 1536);
	}

	#[test]
	fn test_distance_cosine() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Cosine);
		test_distance(h.h.dist, 2000, 1536);
	}

	#[test]
	fn test_distance_euclidean() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Euclidean);
		test_distance(h.h.dist, 2000, 1536);
	}

	#[test]
	fn test_distance_hamming() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Hamming);
		test_distance(h.h.dist, 2000, 1536);
	}

	#[test]
	fn test_distance_jaccard() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Jaccard);
		test_distance(h.h.dist, 1000, 1536);
	}
	#[test]
	fn test_distance_manhattan() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Manhattan);
		test_distance(h.h.dist, 2000, 1536);
	}
	#[test]
	fn test_distance_minkowski() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Minkowski(2.into()));
		test_distance(h.h.dist, 2000, 1536);
	}

	#[test]
	fn test_distance_pearson() {
		let h: HnswIndex<12, 24, 500> = HnswIndex::new(Distance::Pearson);
		test_distance(h.h.dist, 2000, 1536);
	}
}
