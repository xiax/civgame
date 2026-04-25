use bevy::prelude::*;

pub const STATE_DIM:     usize = 24;
pub const PLAN_FEAT_DIM: usize = 8;
pub const Q_IN:          usize = STATE_DIM + PLAN_FEAT_DIM; // 32
const Q_H1:              usize = 16;
const Q_H2:              usize = 8;
const LR:                f32   = 0.005;
const EPSILON:           f32   = 0.10;

/// Per-agent utility Q-network: (state ++ plan_features) → scalar score.
/// Architecture: 32 → 16 → 8 → 1 (ReLU activations).
/// Adding new plans never requires changing the architecture — only their feature vector.
/// ~3 KB per agent.
#[derive(Component, Clone)]
pub struct UtilityNet {
    pub w1: [[f32; Q_IN]; Q_H1],    // 32×16
    pub b1: [f32; Q_H1],
    pub w2: [[f32; Q_H1]; Q_H2],    // 16×8
    pub b2: [f32; Q_H2],
    pub w3: [f32; Q_H2],            // 8×1
    pub b3: f32,
    // Saved at plan dispatch for the learning step
    pub last_input:   [f32; Q_IN],
    pub last_h1:      [f32; Q_H1],
    pub last_h2:      [f32; Q_H2],
    pub last_q:       f32,
    pub last_plan_id: u16,
}

impl Default for UtilityNet {
    fn default() -> Self { Self::new_random() }
}

impl UtilityNet {
    pub fn new_random() -> Self {
        let mut net = UtilityNet {
            w1: [[0.0; Q_IN]; Q_H1],
            b1: [0.0; Q_H1],
            w2: [[0.0; Q_H1]; Q_H2],
            b2: [0.0; Q_H2],
            w3: [0.0; Q_H2],
            b3: 0.0,
            last_input:   [0.0; Q_IN],
            last_h1:      [0.0; Q_H1],
            last_h2:      [0.0; Q_H2],
            last_q:       0.0,
            last_plan_id: 0,
        };
        for i in 0..Q_H1 {
            for j in 0..Q_IN { net.w1[i][j] = (fastrand::f32() - 0.5) * 0.2; }
            net.b1[i] = (fastrand::f32() - 0.5) * 0.2;
        }
        for i in 0..Q_H2 {
            for j in 0..Q_H1 { net.w2[i][j] = (fastrand::f32() - 0.5) * 0.2; }
            net.b2[i] = (fastrand::f32() - 0.5) * 0.2;
        }
        for i in 0..Q_H2 { net.w3[i] = (fastrand::f32() - 0.5) * 0.2; }
        net.b3 = (fastrand::f32() - 0.5) * 0.2;
        net
    }

    /// Inherit weights from one parent with Gaussian mutation (σ≈0.05).
    pub fn from_parent(parent: &UtilityNet) -> Self {
        let mut child = parent.clone();
        child.last_input   = [0.0; Q_IN];
        child.last_h1      = [0.0; Q_H1];
        child.last_h2      = [0.0; Q_H2];
        child.last_q       = 0.0;
        child.last_plan_id = 0;
        for i in 0..Q_H1 {
            for j in 0..Q_IN { child.w1[i][j] += gauss_noise(); }
            child.b1[i] += gauss_noise();
        }
        for i in 0..Q_H2 {
            for j in 0..Q_H1 { child.w2[i][j] += gauss_noise(); }
            child.b2[i] += gauss_noise();
        }
        for i in 0..Q_H2 { child.w3[i] += gauss_noise(); }
        child.b3 += gauss_noise();
        child
    }

    /// Blend two parents' weights with Gaussian mutation.
    pub fn from_parents(mother: &UtilityNet, father: &UtilityNet) -> Self {
        let mut child = UtilityNet {
            w1: [[0.0; Q_IN]; Q_H1],
            b1: [0.0; Q_H1],
            w2: [[0.0; Q_H1]; Q_H2],
            b2: [0.0; Q_H2],
            w3: [0.0; Q_H2],
            b3: 0.0,
            last_input:   [0.0; Q_IN],
            last_h1:      [0.0; Q_H1],
            last_h2:      [0.0; Q_H2],
            last_q:       0.0,
            last_plan_id: 0,
        };
        for i in 0..Q_H1 {
            for j in 0..Q_IN {
                child.w1[i][j] = (mother.w1[i][j] + father.w1[i][j]) * 0.5 + gauss_noise();
            }
            child.b1[i] = (mother.b1[i] + father.b1[i]) * 0.5 + gauss_noise();
        }
        for i in 0..Q_H2 {
            for j in 0..Q_H1 {
                child.w2[i][j] = (mother.w2[i][j] + father.w2[i][j]) * 0.5 + gauss_noise();
            }
            child.b2[i] = (mother.b2[i] + father.b2[i]) * 0.5 + gauss_noise();
        }
        for i in 0..Q_H2 {
            child.w3[i] = (mother.w3[i] + father.w3[i]) * 0.5 + gauss_noise();
        }
        child.b3 = (mother.b3 + father.b3) * 0.5 + gauss_noise();
        child
    }

    /// Score a single plan given the current state. Saves activations for learning.
    pub fn score_plan(
        &mut self,
        state: [f32; STATE_DIM],
        plan_feat: [f32; PLAN_FEAT_DIM],
        plan_id: u16,
    ) -> f32 {
        let mut input = [0.0f32; Q_IN];
        input[..STATE_DIM].copy_from_slice(&state);
        input[STATE_DIM..].copy_from_slice(&plan_feat);
        self.last_input = input;

        let mut h1 = [0.0f32; Q_H1];
        for i in 0..Q_H1 {
            let mut s = self.b1[i];
            for j in 0..Q_IN { s += self.w1[i][j] * input[j]; }
            h1[i] = s.max(0.0); // ReLU
        }
        self.last_h1 = h1;

        let mut h2 = [0.0f32; Q_H2];
        for i in 0..Q_H2 {
            let mut s = self.b2[i];
            for j in 0..Q_H1 { s += self.w2[i][j] * h1[j]; }
            h2[i] = s.max(0.0); // ReLU
        }
        self.last_h2 = h2;

        let mut q = self.b3;
        for i in 0..Q_H2 { q += self.w3[i] * h2[i]; }

        self.last_q = q;
        self.last_plan_id = plan_id;
        q
    }

    /// Stateless evaluation of a plan's score (does not save activations).
    pub fn evaluate_plan(
        &self,
        state: [f32; STATE_DIM],
        plan_feat: [f32; PLAN_FEAT_DIM],
    ) -> f32 {
        let mut input = [0.0f32; Q_IN];
        input[..STATE_DIM].copy_from_slice(&state);
        input[STATE_DIM..].copy_from_slice(&plan_feat);

        let mut h1 = [0.0f32; Q_H1];
        for i in 0..Q_H1 {
            let mut s = self.b1[i];
            for j in 0..Q_IN { s += self.w1[i][j] * input[j]; }
            h1[i] = s.max(0.0);
        }

        let mut h2 = [0.0f32; Q_H2];
        for i in 0..Q_H2 {
            let mut s = self.b2[i];
            for j in 0..Q_H1 { s += self.w2[i][j] * h1[j]; }
            h2[i] = s.max(0.0);
        }

        let mut q = self.b3;
        for i in 0..Q_H2 { q += self.w3[i] * h2[i]; }
        q
    }

    /// ε-greedy plan selection from a slice of (plan_id, score) pairs.
    /// Returns the index into the slice (not the plan_id directly).
    pub fn select_plan_idx(scores: &[(u16, f32)]) -> usize {
        if scores.is_empty() { return 0; }
        if fastrand::f32() < EPSILON {
            fastrand::usize(..scores.len())
        } else {
            scores.iter()
                .enumerate()
                .max_by(|(_, (_, a)), (_, (_, b))| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0)
        }
    }

    /// TD(0) weight update: reward - last_q.
    pub fn learn(&mut self, reward: f32) {
        let delta = reward - self.last_q;
        if delta.abs() < 1e-6 { return; }

        // Output layer gradient
        for i in 0..Q_H2 {
            self.w3[i] += LR * delta * self.last_h2[i];
        }
        self.b3 += LR * delta;

        // Hidden layer 2 gradient (ReLU derivative)
        let mut dh2 = [0.0f32; Q_H2];
        for i in 0..Q_H2 {
            dh2[i] = self.w3[i] * if self.last_h2[i] > 0.0 { 1.0 } else { 0.0 };
            for j in 0..Q_H1 {
                self.w2[i][j] += LR * delta * dh2[i] * self.last_h1[j];
            }
            self.b2[i] += LR * delta * dh2[i];
        }

        // Hidden layer 1 gradient
        let mut dh1 = [0.0f32; Q_H1];
        for j in 0..Q_H1 {
            let mut s = 0.0f32;
            for i in 0..Q_H2 { s += self.w2[i][j] * dh2[i]; }
            dh1[j] = s * if self.last_h1[j] > 0.0 { 1.0 } else { 0.0 };
            for k in 0..Q_IN {
                self.w1[j][k] += LR * delta * dh1[j] * self.last_input[k];
            }
            self.b1[j] += LR * delta * dh1[j];
        }
    }
}

fn gauss_noise() -> f32 {
    (fastrand::f32() + fastrand::f32() - 1.0) * 0.05
}
