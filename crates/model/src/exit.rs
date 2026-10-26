//! Exit codes and mixed-result precedence (docs/apply.md, "Failure and accounting").

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Ok,
    Failed,
    Usage,
    Blocked,
    Declined,
    Pending,
    Interrupted,
    Terminated,
}

impl Exit {
    pub const fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Failed => 1,
            Exit::Usage => 2,
            Exit::Blocked => 3,
            Exit::Declined => 4,
            Exit::Pending => 5,
            Exit::Interrupted => 130,
            Exit::Terminated => 143,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Exit::Ok => "ok",
            Exit::Failed => "failed",
            Exit::Usage => "usage",
            Exit::Blocked => "blocked",
            Exit::Declined => "declined",
            Exit::Pending => "pending",
            Exit::Interrupted => "interrupted",
            Exit::Terminated => "terminated",
        }
    }

    /// Precedence: 143 or 130, then 2, 1, 5, 3, 4, 0.
    const fn rank(self) -> u8 {
        match self {
            Exit::Interrupted | Exit::Terminated => 6,
            Exit::Usage => 5,
            Exit::Failed => 4,
            Exit::Pending => 3,
            Exit::Blocked => 2,
            Exit::Declined => 1,
            Exit::Ok => 0,
        }
    }

    /// Combines two results. On equal precedence the first argument wins, so the
    /// first signal received keeps its code.
    pub fn combine(self, other: Exit) -> Exit {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Exit::*;

    #[test]
    fn codes_match_the_contract() {
        let all = [
            Ok,
            Failed,
            Usage,
            Blocked,
            Declined,
            Pending,
            Interrupted,
            Terminated,
        ];
        let codes: Vec<u8> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes, [0, 1, 2, 3, 4, 5, 130, 143]);
    }

    #[test]
    fn precedence_is_signal_2_1_5_3_4_0() {
        let order = [Terminated, Usage, Failed, Pending, Blocked, Declined, Ok];
        for (i, hi) in order.iter().enumerate() {
            for lo in &order[i + 1..] {
                assert_eq!(hi.combine(*lo), *hi);
                assert_eq!(lo.combine(*hi), *hi);
            }
        }
    }

    #[test]
    fn first_signal_wins() {
        assert_eq!(Interrupted.combine(Terminated), Interrupted);
        assert_eq!(Terminated.combine(Interrupted), Terminated);
    }
}
