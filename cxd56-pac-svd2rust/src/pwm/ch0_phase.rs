///Register `CH0_PHASE` reader
pub type R = crate::R<Ch0PhaseSpec>;
///Register `CH0_PHASE` writer
pub type W = crate::W<Ch0PhaseSpec>;
///Field `PRESCALE` reader - Input clock prescaler exponent (0=÷1, 1=÷2, … 8=÷256)
pub type PrescaleR = crate::FieldReader;
///Field `PRESCALE` writer - Input clock prescaler exponent (0=÷1, 1=÷2, … 8=÷256)
pub type PrescaleW<'a, REG> = crate::FieldWriter<'a, REG, 4>;
impl R {
    ///Bits 16:19 - Input clock prescaler exponent (0=÷1, 1=÷2, … 8=÷256)
    #[inline(always)]
    pub fn prescale(&self) -> PrescaleR {
        PrescaleR::new(((self.bits >> 16) & 0x0f) as u8)
    }
}
impl W {
    ///Bits 16:19 - Input clock prescaler exponent (0=÷1, 1=÷2, … 8=÷256)
    #[inline(always)]
    pub fn prescale(&mut self) -> PrescaleW<'_, Ch0PhaseSpec> {
        PrescaleW::new(self, 16)
    }
}
/**CH0 prescale (bits\[31:16\]=prescale 0-8; clock divisor = 2^prescale)

You can [`read`](crate::Reg::read) this register and get [`ch0_phase::R`](R). You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch0_phase::W`](W). You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).*/
pub struct Ch0PhaseSpec;
impl crate::RegisterSpec for Ch0PhaseSpec {
    type Ux = u32;
}
///`read()` method returns [`ch0_phase::R`](R) reader structure
impl crate::Readable for Ch0PhaseSpec {}
///`write(|w| ..)` method takes [`ch0_phase::W`](W) writer structure
impl crate::Writable for Ch0PhaseSpec {
    type Safety = crate::Unsafe;
}
///`reset()` method sets CH0_PHASE to value 0
impl crate::Resettable for Ch0PhaseSpec {}
