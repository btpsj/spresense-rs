///Register `CH1_PHASE` reader
pub type R = crate::R<Ch1PhaseSpec>;
///Register `CH1_PHASE` writer
pub type W = crate::W<Ch1PhaseSpec>;
///Field `PRESCALE` reader - Input clock prescaler exponent
pub type PrescaleR = crate::FieldReader;
///Field `PRESCALE` writer - Input clock prescaler exponent
pub type PrescaleW<'a, REG> = crate::FieldWriter<'a, REG, 4>;
impl R {
    ///Bits 16:19 - Input clock prescaler exponent
    #[inline(always)]
    pub fn prescale(&self) -> PrescaleR {
        PrescaleR::new(((self.bits >> 16) & 0x0f) as u8)
    }
}
impl W {
    ///Bits 16:19 - Input clock prescaler exponent
    #[inline(always)]
    pub fn prescale(&mut self) -> PrescaleW<'_, Ch1PhaseSpec> {
        PrescaleW::new(self, 16)
    }
}
/**CH1 prescale

You can [`read`](crate::Reg::read) this register and get [`ch1_phase::R`](R). You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_phase::W`](W). You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).*/
pub struct Ch1PhaseSpec;
impl crate::RegisterSpec for Ch1PhaseSpec {
    type Ux = u32;
}
///`read()` method returns [`ch1_phase::R`](R) reader structure
impl crate::Readable for Ch1PhaseSpec {}
///`write(|w| ..)` method takes [`ch1_phase::W`](W) writer structure
impl crate::Writable for Ch1PhaseSpec {
    type Safety = crate::Unsafe;
}
///`reset()` method sets CH1_PHASE to value 0
impl crate::Resettable for Ch1PhaseSpec {}
