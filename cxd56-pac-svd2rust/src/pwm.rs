#[repr(C)]
///Register block
pub struct RegisterBlock {
    ch0_param: Ch0Param,
    ch0_en: Ch0En,
    ch0_update: Ch0Update,
    ch1_param: Ch1Param,
    ch1_en: Ch1En,
    ch1_update: Ch1Update,
    _reserved6: [u8; 0x18],
    ch0_phase: Ch0Phase,
    ch1_phase: Ch1Phase,
}
impl RegisterBlock {
    ///0x00 - CH0 period and off-period (bits\[15:0\]=period, bits\[31:16\]=offperiod)
    #[inline(always)]
    pub const fn ch0_param(&self) -> &Ch0Param {
        &self.ch0_param
    }
    ///0x04 - CH0 output enable
    #[inline(always)]
    pub const fn ch0_en(&self) -> &Ch0En {
        &self.ch0_en
    }
    ///0x08 - CH0 update trigger (write any value to apply PARAM changes)
    #[inline(always)]
    pub const fn ch0_update(&self) -> &Ch0Update {
        &self.ch0_update
    }
    ///0x0c - CH1 period and off-period
    #[inline(always)]
    pub const fn ch1_param(&self) -> &Ch1Param {
        &self.ch1_param
    }
    ///0x10 - CH1 output enable
    #[inline(always)]
    pub const fn ch1_en(&self) -> &Ch1En {
        &self.ch1_en
    }
    ///0x14 - CH1 update trigger
    #[inline(always)]
    pub const fn ch1_update(&self) -> &Ch1Update {
        &self.ch1_update
    }
    ///0x30 - CH0 prescale (bits\[31:16\]=prescale 0-8; clock divisor = 2^prescale)
    #[inline(always)]
    pub const fn ch0_phase(&self) -> &Ch0Phase {
        &self.ch0_phase
    }
    ///0x34 - CH1 prescale
    #[inline(always)]
    pub const fn ch1_phase(&self) -> &Ch1Phase {
        &self.ch1_phase
    }
}
/**CH0_PARAM (rw) register accessor: CH0 period and off-period (bits\[15:0\]=period, bits\[31:16\]=offperiod)

You can [`read`](crate::Reg::read) this register and get [`ch0_param::R`]. You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_param::W`]. You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch0_param`] module*/
#[doc(alias = "CH0_PARAM")]
pub type Ch0Param = crate::Reg<ch0_param::Ch0ParamSpec>;
///CH0 period and off-period (bits\[15:0\]=period, bits\[31:16\]=offperiod)
pub mod ch0_param;
/**CH0_EN (rw) register accessor: CH0 output enable

You can [`read`](crate::Reg::read) this register and get [`ch0_en::R`]. You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_en::W`]. You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch0_en`] module*/
#[doc(alias = "CH0_EN")]
pub type Ch0En = crate::Reg<ch0_en::Ch0EnSpec>;
///CH0 output enable
pub mod ch0_en;
/**CH0_UPDATE (w) register accessor: CH0 update trigger (write any value to apply PARAM changes)

You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_update::W`]. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch0_update`] module*/
#[doc(alias = "CH0_UPDATE")]
pub type Ch0Update = crate::Reg<ch0_update::Ch0UpdateSpec>;
///CH0 update trigger (write any value to apply PARAM changes)
pub mod ch0_update;
/**CH1_PARAM (rw) register accessor: CH1 period and off-period

You can [`read`](crate::Reg::read) this register and get [`ch1_param::R`]. You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_param::W`]. You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch1_param`] module*/
#[doc(alias = "CH1_PARAM")]
pub type Ch1Param = crate::Reg<ch1_param::Ch1ParamSpec>;
///CH1 period and off-period
pub mod ch1_param;
/**CH1_EN (rw) register accessor: CH1 output enable

You can [`read`](crate::Reg::read) this register and get [`ch1_en::R`]. You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_en::W`]. You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch1_en`] module*/
#[doc(alias = "CH1_EN")]
pub type Ch1En = crate::Reg<ch1_en::Ch1EnSpec>;
///CH1 output enable
pub mod ch1_en;
/**CH1_UPDATE (w) register accessor: CH1 update trigger

You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_update::W`]. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch1_update`] module*/
#[doc(alias = "CH1_UPDATE")]
pub type Ch1Update = crate::Reg<ch1_update::Ch1UpdateSpec>;
///CH1 update trigger
pub mod ch1_update;
/**CH0_PHASE (rw) register accessor: CH0 prescale (bits\[31:16\]=prescale 0-8; clock divisor = 2^prescale)

You can [`read`](crate::Reg::read) this register and get [`ch0_phase::R`]. You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_phase::W`]. You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch0_phase`] module*/
#[doc(alias = "CH0_PHASE")]
pub type Ch0Phase = crate::Reg<ch0_phase::Ch0PhaseSpec>;
///CH0 prescale (bits\[31:16\]=prescale 0-8; clock divisor = 2^prescale)
pub mod ch0_phase;
/**CH1_PHASE (rw) register accessor: CH1 prescale

You can [`read`](crate::Reg::read) this register and get [`ch1_phase::R`]. You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_phase::W`]. You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).

For information about available fields see [`mod@ch1_phase`] module*/
#[doc(alias = "CH1_PHASE")]
pub type Ch1Phase = crate::Reg<ch1_phase::Ch1PhaseSpec>;
///CH1 prescale
pub mod ch1_phase;
