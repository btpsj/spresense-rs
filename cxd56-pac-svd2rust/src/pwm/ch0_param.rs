///Register `CH0_PARAM` reader
pub type R = crate::R<Ch0ParamSpec>;
///Register `CH0_PARAM` writer
pub type W = crate::W<Ch0ParamSpec>;
///Field `PERIOD` reader - PWM cycle period count (16-bit counter, 1-65535)
pub type PeriodR = crate::FieldReader<u16>;
///Field `PERIOD` writer - PWM cycle period count (16-bit counter, 1-65535)
pub type PeriodW<'a, REG> = crate::FieldWriter<'a, REG, 16, u16>;
///Field `OFFPERIOD` reader - Off-period (LOW time) count; on-period = PERIOD - OFFPERIOD
pub type OffperiodR = crate::FieldReader<u16>;
///Field `OFFPERIOD` writer - Off-period (LOW time) count; on-period = PERIOD - OFFPERIOD
pub type OffperiodW<'a, REG> = crate::FieldWriter<'a, REG, 16, u16>;
impl R {
    ///Bits 0:15 - PWM cycle period count (16-bit counter, 1-65535)
    #[inline(always)]
    pub fn period(&self) -> PeriodR {
        PeriodR::new((self.bits & 0xffff) as u16)
    }
    ///Bits 16:31 - Off-period (LOW time) count; on-period = PERIOD - OFFPERIOD
    #[inline(always)]
    pub fn offperiod(&self) -> OffperiodR {
        OffperiodR::new(((self.bits >> 16) & 0xffff) as u16)
    }
}
impl W {
    ///Bits 0:15 - PWM cycle period count (16-bit counter, 1-65535)
    #[inline(always)]
    pub fn period(&mut self) -> PeriodW<'_, Ch0ParamSpec> {
        PeriodW::new(self, 0)
    }
    ///Bits 16:31 - Off-period (LOW time) count; on-period = PERIOD - OFFPERIOD
    #[inline(always)]
    pub fn offperiod(&mut self) -> OffperiodW<'_, Ch0ParamSpec> {
        OffperiodW::new(self, 16)
    }
}
/**CH0 period and off-period (bits\[15:0\]=period, bits\[31:16\]=offperiod)

You can [`read`](crate::Reg::read) this register and get [`ch0_param::R`](R). You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_param::W`](W). You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).*/
pub struct Ch0ParamSpec;
impl crate::RegisterSpec for Ch0ParamSpec {
    type Ux = u32;
}
///`read()` method returns [`ch0_param::R`](R) reader structure
impl crate::Readable for Ch0ParamSpec {}
///`write(|w| ..)` method takes [`ch0_param::W`](W) writer structure
impl crate::Writable for Ch0ParamSpec {
    type Safety = crate::Unsafe;
}
///`reset()` method sets CH0_PARAM to value 0
impl crate::Resettable for Ch0ParamSpec {}
